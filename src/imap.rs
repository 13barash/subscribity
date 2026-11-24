//! Read-only IMAP client for the mail scanner: LOGIN, CAPABILITY, LIST, EXAMINE,
//! UID SEARCH (X-GM-RAW on Gmail), UID FETCH with BODY.PEEK only. Never SELECT/STORE/EXPUNGE.
//! Generic over the stream so tests run on recorded transcripts. Contents are never logged.

use crate::data::{civil_from_days, days_from_civil};
use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Bytes kept per literal; the rest is read and dropped so the parser stays in sync.
const LIT_CAP: usize = 64 * 1024;
/// Total bytes read per session before giving up.
pub const SESSION_CAP: usize = 40 * 1024 * 1024;
const LINE_CAP: usize = 4 * 1024 * 1024;
const BATCH: usize = 25;
const GM_MAX: usize = 900;
const TERMS: usize = 20;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const FETCH_ITEMS: &str =
    "(UID INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT CONTENT-TYPE CONTENT-TRANSFER-ENCODING)] BODY.PEEK[TEXT]<0.49152>)";

#[cfg_attr(test, derive(Debug))]
pub enum ImapError {
    Auth(String),
    ImapOff,
    Tls(String),
    Net(String),
    Cancelled,
    Proto(String),
}

impl ImapError {
    /// Plain text for the user.
    pub fn message(&self) -> String {
        match self {
            ImapError::Auth(e) if e == "invalid characters" => "Enter your email address and app password (no line breaks or control characters).".into(),
            ImapError::Auth(_) => "Wrong email or app password. Use an app password (it needs 2-Step Verification), not your normal password.".into(),
            ImapError::ImapOff => "IMAP is turned off for this mailbox. Turn it on in your mail settings, then try again.".into(),
            ImapError::Tls(e) if e == "certificate check failed" => {
                "The mail server's certificate could not be verified, so Subscribity did not connect.".into()
            }
            ImapError::Tls(e) => format!("Secure connection failed: {}.", e),
            ImapError::Net(e) if e.contains("timed out") => "The mail server did not answer in time. Try again later.".into(),
            ImapError::Net(e) => format!("Could not talk to the mail server ({}).", e),
            ImapError::Cancelled => "Scan cancelled.".into(),
            ImapError::Proto(e) => format!("The mail server sent an unexpected answer ({}).", e),
        }
    }
}

fn io_err(e: std::io::Error) -> ImapError {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => ImapError::Net("timed out".into()),
        ErrorKind::InvalidData => ImapError::Tls(e.to_string()), // raised by tls.rs
        _ => ImapError::Net("connection lost".into()),
    }
}

fn proto(s: &str) -> ImapError {
    ImapError::Proto(s.into())
}

/// One fetched message, raw bytes (MIME decoding is mime.rs's job).
pub struct Mail {
    pub uid: u32,
    /// Day number (data.rs epoch) of INTERNALDATE, in that date's own time zone.
    pub date: i32,
    pub header: Vec<u8>,
    pub body: Vec<u8>,
}

enum Tok {
    A(Vec<u8>), // atom (incl. NIL, numbers, `BODY[...]<0>`, `[CODE ...]`)
    S(Vec<u8>), // quoted string or literal
    Open,
    Close,
}

fn is(t: &Tok, w: &str) -> bool {
    matches!(t, Tok::A(a) if a.eq_ignore_ascii_case(w.as_bytes()))
}

fn num(b: &[u8]) -> Option<u32> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// Splits response text into tokens. Brackets keep `BODY[HEADER.FIELDS (A B)]` as one atom.
fn lex(t: &[u8], v: &mut Vec<Tok>) {
    let mut i = 0;
    while i < t.len() {
        match t[i] {
            b' ' => i += 1,
            b'(' => {
                v.push(Tok::Open);
                i += 1
            }
            b')' => {
                v.push(Tok::Close);
                i += 1
            }
            b'"' => {
                i += 1;
                let mut s = Vec::new();
                while i < t.len() && t[i] != b'"' {
                    if t[i] == b'\\' && i + 1 < t.len() {
                        i += 1;
                    }
                    s.push(t[i]);
                    i += 1;
                }
                i += 1;
                v.push(Tok::S(s));
            }
            _ => {
                let st = i;
                let mut depth = 0;
                while i < t.len() {
                    let c = t[i];
                    if c == b'[' {
                        depth += 1;
                    } else if c == b']' && depth > 0 {
                        depth -= 1;
                    } else if depth == 0 && matches!(c, b' ' | b'(' | b')' | b'"') {
                        break;
                    }
                    i += 1;
                }
                v.push(Tok::A(t[st..i].to_vec()));
            }
        }
    }
}

/// Text of a status line (atoms and strings joined by spaces), for error matching only.
fn text(toks: &[Tok]) -> String {
    let mut s = Vec::new();
    for t in toks {
        if let Tok::A(b) | Tok::S(b) = t {
            if !s.is_empty() {
                s.push(b' ');
            }
            s.extend_from_slice(b);
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

/// `{n}` or `{n+}` at the end of a line: returns the text before it and n.
fn literal_suffix(l: &[u8]) -> Option<(&[u8], usize)> {
    if l.last() != Some(&b'}') {
        return None;
    }
    let open = l.iter().rposition(|&c| c == b'{')?;
    let inner = &l[open + 1..l.len() - 1];
    let inner = inner.strip_suffix(b"+").unwrap_or(inner);
    if inner.is_empty() || inner.len() > 10 || !inner.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((&l[..open], std::str::from_utf8(inner).ok()?.parse().ok()?))
}

/// A command; every piece but the last ends with `{n}` and the next one starts with those n bytes.
struct Cmd {
    pieces: Vec<Vec<u8>>,
}

impl Cmd {
    fn new(s: &str) -> Cmd {
        Cmd { pieces: vec![s.as_bytes().to_vec()] }
    }

    fn raw(mut self, s: &str) -> Cmd {
        self.pieces.last_mut().unwrap().extend_from_slice(s.as_bytes());
        self
    }

    /// Appends " string": quoted when printable ASCII, else a synchronizing literal.
    fn string(mut self, v: &[u8]) -> Cmd {
        let last = self.pieces.last_mut().unwrap();
        last.push(b' ');
        if v.len() < 1000 && v.iter().all(|&c| (0x20..0x7f).contains(&c)) {
            last.push(b'"');
            for &c in v {
                if c == b'"' || c == b'\\' {
                    last.push(b'\\');
                }
                last.push(c);
            }
            last.push(b'"');
        } else {
            last.extend_from_slice(format!("{{{}}}", v.len()).as_bytes());
            self.pieces.push(v.to_vec());
        }
        self
    }
}

/// The tagged answer plus the untagged lines that came before it.
struct Done {
    ok: bool,
    bad: bool,
    text: String,
    untagged: Vec<Vec<Tok>>,
}

enum Step {
    Untagged,
    Cont,
    Tagged(Done),
}

pub struct Session<S: Read + Write> {
    s: S,
    buf: Vec<u8>,
    pos: usize,
    total: usize,
    tag: u32,
    caps: String,
    closing: bool,
}

/// Gmail shows app passwords as "abcd efgh ijkl mnop": drop the spaces when that is the shape.
fn app_password(p: &str) -> String {
    let q: String = p.chars().filter(|&c| c != ' ').collect();
    if q.len() == 16 && q.bytes().all(|b| b.is_ascii_alphabetic()) {
        q
    } else {
        p.to_string()
    }
}

fn login_error(d: &Done) -> ImapError {
    let mut t = d.text.to_ascii_lowercase();
    for l in &d.untagged {
        t.push(' ');
        t.push_str(&text(l).to_ascii_lowercase());
    }
    let short: String = d.text.chars().take(200).collect();
    if t.contains("authenticationfailed") || t.contains("application-specific password") || t.contains("invalid credentials") {
        ImapError::Auth(short)
    } else if t.contains("imap access") || t.contains("enabled for imap") || (t.contains("[alert]") && t.contains("imap")) {
        ImapError::ImapOff
    } else if d.bad {
        ImapError::Proto(short)
    } else {
        ImapError::Auth(short)
    }
}

fn parse_date(b: &[u8]) -> Option<i32> {
    // "dd-Mon-yyyy hh:mm:ss +zzzz", day may be space padded
    let s = std::str::from_utf8(b).ok()?.trim();
    let mut p = s.splitn(3, '-');
    let d: u32 = p.next()?.trim().parse().ok()?;
    let mon = p.next()?;
    let y: i32 = p.next()?.get(..4)?.parse().ok()?;
    let m = MONTHS.iter().position(|x| x.eq_ignore_ascii_case(mon))? as u32 + 1;
    if !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// `* n FETCH (...)` → Mail, if it carries a UID and at least one body part.
fn parse_fetch(l: Vec<Tok>) -> Option<Mail> {
    if l.len() < 4 || !is(&l[2], "FETCH") || !matches!(l[3], Tok::Open) {
        return None;
    }
    let mut m = Mail { uid: 0, date: 0, header: Vec::new(), body: Vec::new() };
    let mut got = false;
    let mut it = l.into_iter().skip(4);
    while let Some(k) = it.next() {
        let key = match k {
            Tok::A(a) => a.to_ascii_uppercase(),
            Tok::Close => break,
            _ => continue,
        };
        let v = match it.next()? {
            Tok::Open => {
                let mut depth = 1;
                while depth > 0 {
                    match it.next()? {
                        Tok::Open => depth += 1,
                        Tok::Close => depth -= 1,
                        _ => {}
                    }
                }
                continue;
            }
            Tok::Close => break,
            Tok::A(b) if b.eq_ignore_ascii_case(b"NIL") => Vec::new(),
            Tok::A(b) | Tok::S(b) => b,
        };
        if key == b"UID" {
            m.uid = num(&v)?;
        } else if key == b"INTERNALDATE" {
            m.date = parse_date(&v).unwrap_or(0);
        } else if key.starts_with(b"BODY[HEADER") {
            m.header = v;
            got = true;
        } else if key.starts_with(b"BODY[TEXT]") {
            m.body = v;
            got = true;
        }
    }
    if m.uid == 0 || !got {
        None
    } else {
        Some(m)
    }
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '"' | '(' | ')' | '\\' | '\r' | '\n')).collect::<String>().trim().to_string()
}

fn gm_query(date: &str, from: &[String], subj: &[String]) -> String {
    let f = format!("from:({})", from.join(" OR "));
    let s = format!("subject:({})", subj.join(" OR "));
    match (from.is_empty(), subj.is_empty()) {
        (false, false) => format!("after:{} ({} OR {})", date, f, s),
        (false, true) => format!("after:{} {}", date, f),
        (true, false) => format!("after:{} {}", date, s),
        (true, true) => format!("after:{}", date),
    }
}

/// X-GM-RAW queries, each ≤ ~GM_MAX bytes; ASCII and non-ASCII terms never share a query.
fn gmail_queries(date: &str, senders: &[&str], keywords: &[&str]) -> Vec<String> {
    let mut items: Vec<(bool, String)> = Vec::new();
    for s in senders {
        let s = clean(s).replace(' ', "");
        if !s.is_empty() {
            items.push((true, s));
        }
    }
    for k in keywords {
        let k = clean(k);
        if !k.is_empty() {
            items.push((false, if k.contains(' ') { format!("\"{}\"", k) } else { k }));
        }
    }
    let mut out = Vec::new();
    for ascii in [true, false] {
        let (mut from, mut subj) = (Vec::new(), Vec::new());
        for (is_from, term) in items.iter().filter(|(_, t)| t.is_ascii() == ascii) {
            let list = if *is_from { &mut from } else { &mut subj };
            list.push(term.clone());
            if gm_query(date, &from, &subj).len() > GM_MAX && from.len() + subj.len() > 1 {
                let last = if *is_from { from.pop() } else { subj.pop() };
                out.push(gm_query(date, &from, &subj));
                from.clear();
                subj.clear();
                if *is_from { from.extend(last) } else { subj.extend(last) }
            }
        }
        if !from.is_empty() || !subj.is_empty() {
            out.push(gm_query(date, &from, &subj));
        }
    }
    if out.is_empty() {
        out.push(gm_query(date, &[], &[]));
    }
    out
}

/// Standard SEARCH commands: `SINCE d OR OR k1 k2 k3`, ≤ TERMS keys each.
fn std_searches(date: &str, senders: &[&str], keywords: &[&str]) -> Vec<Cmd> {
    let mut terms: Vec<(&str, String)> = Vec::new();
    terms.extend(senders.iter().map(|s| ("FROM", clean(s))).filter(|t| !t.1.is_empty()));
    terms.extend(keywords.iter().map(|k| ("SUBJECT", clean(k))).filter(|t| !t.1.is_empty()));
    let mut out = Vec::new();
    for ascii in [true, false] {
        let group: Vec<&(&str, String)> = terms.iter().filter(|t| t.1.is_ascii() == ascii).collect();
        for chunk in group.chunks(TERMS) {
            let head = if ascii { "UID SEARCH SINCE " } else { "UID SEARCH CHARSET UTF-8 SINCE " };
            let mut c = Cmd::new(&format!("{}{}", head, date));
            for _ in 1..chunk.len() {
                c = c.raw(" OR");
            }
            for (k, v) in chunk {
                c = c.raw(" ").raw(k).string(v.as_bytes());
            }
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push(Cmd::new(&format!("UID SEARCH SINCE {}", date)));
    }
    out
}

impl<S: Read + Write> Session<S> {
    /// Reads the greeting, logs in once (LOGIN), then reads CAPABILITY.
    pub fn login(stream: S, user: &str, pass: &str) -> Result<Session<S>, ImapError> {
        let mut s = Session { s: stream, buf: Vec::new(), pos: 0, total: 0, tag: 0, caps: String::new(), closing: false };
        let g = s.read_line()?;
        if g.len() < 2 || !is(&g[0], "*") {
            return Err(proto("no greeting"));
        }
        if is(&g[1], "BYE") {
            return Err(ImapError::Net("server refused the connection".into()));
        }
        if !is(&g[1], "PREAUTH") {
            let user = user.trim();
            let pass = app_password(pass);
            let ctl = |t: &str| t.bytes().any(|b| b < 0x20 || b == 0x7f);
            if user.is_empty() || pass.is_empty() || ctl(user) || ctl(&pass) {
                return Err(ImapError::Auth("invalid characters".into()));
            }
            let d = s.run(&Cmd::new("LOGIN").string(user.as_bytes()).string(pass.as_bytes()))?;
            if !d.ok {
                return Err(login_error(&d));
            }
        }
        let d = s.run(&Cmd::new("CAPABILITY"))?;
        for l in d.untagged.iter().filter(|l| l.len() > 1 && is(&l[1], "CAPABILITY")) {
            s.caps.push_str(&text(&l[2..]).to_ascii_uppercase());
            s.caps.push(' ');
        }
        Ok(s)
    }

    pub fn is_gmail(&self) -> bool {
        self.caps.split(' ').any(|c| c == "X-GM-EXT-1")
    }

    /// Opens the `\All` mailbox (else INBOX) read-only. Returns (name as the server spells it, UIDVALIDITY).
    pub fn examine_all(&mut self) -> Result<(String, u32), ImapError> {
        let d = self.run(&Cmd::new("LIST").string(b"").string(b"*"))?;
        if !d.ok {
            return Err(proto("mailbox list refused"));
        }
        let mut pick = None;
        for l in d.untagged {
            if l.len() < 4 || !is(&l[1], "LIST") || !matches!(l[2], Tok::Open) {
                continue;
            }
            let Some(close) = l.iter().position(|t| matches!(t, Tok::Close)) else { continue };
            let flags = &l[3..close];
            let all = flags.iter().any(|t| is(t, "\\All"));
            let skip = flags.iter().any(|t| is(t, "\\Noselect") || is(t, "\\NonExistent"));
            if all && !skip {
                if let Some(Tok::A(n) | Tok::S(n)) = l.into_iter().nth(close + 2) {
                    pick = Some(n);
                    break;
                }
            }
        }
        let name = pick.unwrap_or_else(|| b"INBOX".to_vec());
        let d = self.run(&Cmd::new("EXAMINE").string(&name))?;
        if !d.ok {
            return Err(proto("mailbox could not be opened"));
        }
        let mut uv = 0;
        for l in &d.untagged {
            for t in l {
                if let Tok::A(a) = t {
                    if a.len() > 13 && a[..13].eq_ignore_ascii_case(b"[UIDVALIDITY ") {
                        uv = num(a[13..].strip_suffix(b"]").unwrap_or(&a[13..])).unwrap_or(0);
                    }
                }
            }
        }
        Ok((String::from_utf8_lossy(&name).into_owned(), uv))
    }

    /// UIDs (> after_uid, newest first) of mail since `since_days` from any sender or with any subject keyword.
    pub fn search(&mut self, since_days: i64, senders: &[&str], keywords: &[&str], after_uid: u32) -> Result<Vec<u32>, ImapError> {
        let (y, m, d) = civil_from_days(since_days.clamp(0, i32::MAX as i64) as i32);
        let cmds: Vec<Cmd> = if self.is_gmail() {
            let date = format!("{}/{:02}/{:02}", y, m, d);
            // Plus Gmail's own "Purchases" category (empty or off for some users: harmless).
            gmail_queries(&date, senders, keywords)
                .into_iter()
                .chain(std::iter::once(format!("after:{} category:purchases", date)))
                .map(|q| {
                    let head = if q.is_ascii() { "UID SEARCH X-GM-RAW" } else { "UID SEARCH CHARSET UTF-8 X-GM-RAW" };
                    Cmd::new(head).string(q.as_bytes())
                })
                .collect()
        } else {
            std_searches(&format!("{}-{}-{}", d, MONTHS[m as usize - 1], y), senders, keywords)
        };
        let mut set = BTreeSet::new();
        for c in &cmds {
            let d = self.run(c)?;
            if !d.ok {
                if d.text.to_ascii_uppercase().contains("[BADCHARSET") {
                    continue; // server can't search non-ASCII text: skip those terms
                }
                return Err(proto("search refused"));
            }
            for l in d.untagged.iter().filter(|l| l.len() > 1 && is(&l[1], "SEARCH")) {
                for t in &l[2..] {
                    if let Some(u) = match t { Tok::A(a) => num(a), _ => None } {
                        if u > after_uid {
                            set.insert(u);
                        }
                    }
                }
            }
        }
        Ok(set.into_iter().rev().collect())
    }

    /// Fetches headers + first 48 KB of text per UID, 25 at a time, calling `f` per message.
    pub fn fetch(&mut self, uids: &[u32], cancel: &AtomicBool, f: &mut dyn FnMut(Mail)) -> Result<(), ImapError> {
        for batch in uids.chunks(BATCH) {
            if cancel.load(Ordering::Relaxed) {
                return Err(ImapError::Cancelled);
            }
            let set: Vec<String> = batch.iter().map(|u| u.to_string()).collect();
            let d = match self.run(&Cmd::new(&format!("UID FETCH {} {}", set.join(","), FETCH_ITEMS))) {
                Ok(d) => d,
                Err(_) if cancel.load(Ordering::Relaxed) => return Err(ImapError::Cancelled),
                Err(e) => return Err(e),
            };
            if d.bad {
                return Err(proto("fetch refused"));
            }
            // A NO ("some messages could not be fetched") still delivers what came.
            for l in d.untagged {
                if let Some(m) = parse_fetch(l) {
                    f(m);
                }
            }
        }
        Ok(())
    }

    pub fn logout(mut self) {
        self.closing = true;
        let _ = self.run(&Cmd::new("LOGOUT"));
    }

    // ---------- wire ----------

    fn fill(&mut self) -> Result<(), ImapError> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let len = self.buf.len();
        self.buf.resize(len + 16 * 1024, 0);
        let r = loop {
            match self.s.read(&mut self.buf[len..]) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        let n = *r.as_ref().unwrap_or(&0);
        self.buf.truncate(len + n);
        let n = r.map_err(io_err)?;
        if n == 0 {
            return Err(ImapError::Net("connection closed".into()));
        }
        self.total += n;
        if self.total > SESSION_CAP {
            return Err(proto("too much data"));
        }
        Ok(())
    }

    /// One line without CRLF.
    fn raw_line(&mut self) -> Result<Vec<u8>, ImapError> {
        let mut scanned = 0;
        loop {
            if let Some(i) = self.buf[self.pos + scanned..].iter().position(|&c| c == b'\n') {
                let end = self.pos + scanned + i;
                let mut l = self.buf[self.pos..end].to_vec();
                if l.last() == Some(&b'\r') {
                    l.pop();
                }
                self.pos = end + 1;
                return Ok(l);
            }
            scanned = self.buf.len() - self.pos;
            if scanned > LINE_CAP {
                return Err(proto("line too long"));
            }
            self.fill()?;
        }
    }

    /// n literal bytes; keeps the first LIT_CAP.
    fn read_lit(&mut self, n: usize) -> Result<Vec<u8>, ImapError> {
        let mut out = Vec::with_capacity(n.min(LIT_CAP));
        let mut left = n;
        while left > 0 {
            if self.pos == self.buf.len() {
                self.fill()?;
            }
            let take = left.min(self.buf.len() - self.pos);
            let keep = take.min(LIT_CAP - out.len());
            out.extend_from_slice(&self.buf[self.pos..self.pos + keep]);
            self.pos += take;
            left -= take;
        }
        Ok(out)
    }

    /// One logical response line, literals inlined as string tokens.
    fn read_line(&mut self) -> Result<Vec<Tok>, ImapError> {
        let mut toks = Vec::new();
        let mut size = 0;
        loop {
            let line = self.raw_line()?;
            size += line.len();
            if size > LINE_CAP {
                return Err(proto("line too long"));
            }
            match literal_suffix(&line) {
                Some((head, n)) => {
                    lex(head, &mut toks);
                    let lit = self.read_lit(n)?;
                    toks.push(Tok::S(lit));
                }
                None => {
                    lex(&line, &mut toks);
                    return Ok(toks);
                }
            }
        }
    }

    /// Reads one line: untagged lines are stored, "+" and our tagged answer are returned.
    fn step(&mut self, tag: &str, untagged: &mut Vec<Vec<Tok>>) -> Result<Step, ImapError> {
        let l = self.read_line()?;
        if l.is_empty() {
            return Err(proto("empty line"));
        }
        if is(&l[0], "*") {
            if l.len() > 1 && is(&l[1], "BYE") && !self.closing {
                return Err(ImapError::Net("server closed the connection".into()));
            }
            untagged.push(l);
            return Ok(Step::Untagged);
        }
        if is(&l[0], "+") {
            return Ok(Step::Cont);
        }
        if !is(&l[0], tag) || l.len() < 2 {
            return Err(proto("unexpected answer"));
        }
        Ok(Step::Tagged(Done { ok: is(&l[1], "OK"), bad: is(&l[1], "BAD"), text: text(&l[2..]), untagged: Vec::new() }))
    }

    /// Sends a command (waiting for "+" before each literal) and reads up to its tagged answer.
    fn run(&mut self, c: &Cmd) -> Result<Done, ImapError> {
        self.tag += 1;
        let tag = format!("A{}", self.tag);
        let mut untagged = Vec::new();
        for (i, p) in c.pieces.iter().enumerate() {
            let mut line = Vec::with_capacity(p.len() + tag.len() + 3);
            if i == 0 {
                line.extend_from_slice(tag.as_bytes());
                line.push(b' ');
            }
            line.extend_from_slice(p);
            line.extend_from_slice(b"\r\n");
            self.s.write_all(&line).map_err(io_err)?;
            self.s.flush().map_err(io_err)?;
            if i + 1 < c.pieces.len() {
                loop {
                    match self.step(&tag, &mut untagged)? {
                        Step::Untagged => {}
                        Step::Cont => break,
                        Step::Tagged(mut d) => {
                            d.untagged = untagged; // answered instead of "+": literal refused
                            return Ok(d);
                        }
                    }
                }
            }
        }
        loop {
            match self.step(&tag, &mut untagged)? {
                Step::Untagged => {}
                Step::Cont => return Err(proto("unexpected continuation")),
                Step::Tagged(mut d) => {
                    d.untagged = untagged;
                    return Ok(d);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted server: replays `input` in reads of at most `chunk` bytes, records what the client sent.
    struct Script {
        input: Vec<u8>,
        pos: usize,
        chunk: usize,
        output: Vec<u8>,
    }

    impl Read for Script {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let n = b.len().min(self.chunk).min(self.input.len() - self.pos);
            b[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for Script {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Fixture text -> wire bytes: CRLF line ends; `<<<\n...\n>>>` becomes `{n}\r\n...` (n computed).
    fn load(fx: &[u8]) -> Vec<u8> {
        let s = String::from_utf8(fx.to_vec()).unwrap().replace("\r\n", "\n");
        let mut out = String::new();
        let mut rest = s.as_str();
        while let Some(i) = rest.find("<<<\n") {
            out.push_str(&rest[..i].replace('\n', "\r\n"));
            let after = &rest[i + 4..];
            let j = after.find("\n>>>").expect("unclosed literal");
            let body = after[..j].replace('\n', "\r\n");
            out.push_str(&format!("{{{}}}\r\n{}", body.len(), body));
            rest = &after[j + 4..];
        }
        out.push_str(&rest.replace('\n', "\r\n"));
        out.into_bytes()
    }

    fn script(input: Vec<u8>, chunk: usize) -> Script {
        Script { input, pos: 0, chunk, output: Vec::new() }
    }

    fn out(s: &Session<Script>) -> String {
        String::from_utf8_lossy(&s.s.output).into_owned()
    }

    fn assert_read_only(out: &str) {
        let o = out.to_ascii_uppercase();
        for bad in ["STORE", "SELECT ", "EXPUNGE", "COPY", "MOVE", "APPEND", "DELETE", "RFC822", "BODY["] {
            assert!(!o.contains(bad), "sent {}", bad);
        }
    }

    const GMAIL: &[u8] = include_bytes!("fixtures/gmail.imap.txt");
    const GENERIC: &[u8] = include_bytes!("fixtures/generic.imap.txt");
    const AUTH_FAIL: &[u8] = include_bytes!("fixtures/auth_fail.imap.txt");
    const BYE: &[u8] = include_bytes!("fixtures/bye.imap.txt");
    const HEB: &str = "\u{05E7}\u{05D1}\u{05DC}\u{05D4}"; // Hebrew "receipt"

    #[test]
    fn imap_gmail_session() {
        // Big reads and 5-byte reads (every literal split mid-way) must give the same result.
        for chunk in [1 << 20, 5] {
            let mut s = Session::login(script(load(GMAIL), chunk), "test.user@example.com", "abcd efgh ijkl mnop").unwrap();
            assert!(s.is_gmail());
            let (mb, uv) = s.examine_all().unwrap();
            assert_eq!(mb, "[Gmail]/&BdsF3AAg-&BdQF0wXVBdAF6A-");
            assert_eq!(uv, 7);
            let since = days_from_civil(2025, 11, 24) as i64;
            let uids = s.search(since, &["netflix.com", "spotify.com"], &["receipt", "free trial", HEB], 100).unwrap();
            assert_eq!(uids, vec![250, 240, 230, 105, 101]);
            let mut mails = Vec::new();
            s.fetch(&uids, &AtomicBool::new(false), &mut |m| mails.push(m)).unwrap();
            assert_eq!(mails.len(), 3);
            assert_eq!(mails[0].uid, 240);
            assert_eq!(mails[0].date, days_from_civil(2026, 11, 4));
            assert!(mails[0].header.starts_with(b"From: Netflix <info@account.netflix.com>\r\nSubject:"));
            assert!(mails[0].header.ends_with(b"utf-8\r\n\r\n"));
            assert_eq!(mails[0].body, b"Total charged: $15.49\r\nNext billing date: December 4, 2026");
            assert_eq!((mails[1].uid, mails[1].date), (230, days_from_civil(2026, 10, 1)));
            assert!(mails[1].body.starts_with(b"Thanks for your payment of 29.90 ILS (with \"quotes\""));
            assert!(mails[1].header.starts_with(b"From: Spotify"));
            assert_eq!((mails[2].uid, mails[2].header.len(), mails[2].body.len()), (105, 0, 0));
            let o = out(&s);
            let q = format!("after:2025/11/24 subject:({})", HEB);
            let want = format!(
                "A1 LOGIN \"test.user@example.com\" \"abcdefghijklmnop\"\r\n\
                 A2 CAPABILITY\r\n\
                 A3 LIST \"\" \"*\"\r\n\
                 A4 EXAMINE \"[Gmail]/&BdsF3AAg-&BdQF0wXVBdAF6A-\"\r\n\
                 A5 UID SEARCH X-GM-RAW \"after:2025/11/24 (from:(netflix.com OR spotify.com) OR subject:(receipt OR \\\"free trial\\\"))\"\r\n\
                 A6 UID SEARCH CHARSET UTF-8 X-GM-RAW {{{}}}\r\n{}\r\n\
                 A7 UID SEARCH X-GM-RAW \"after:2025/11/24 category:purchases\"\r\n\
                 A8 UID FETCH 250,240,230,105,101 {}\r\n",
                q.len(),
                q,
                FETCH_ITEMS
            );
            assert_eq!(o, want);
            assert_read_only(&o);
            s.logout();
        }
    }

    /// Checks `OR a b` nesting: one search key must consume all tokens.
    fn key(t: &[&str], i: &mut usize) -> bool {
        match t.get(*i) {
            Some(&"OR") => {
                *i += 1;
                key(t, i) && key(t, i)
            }
            Some(&"FROM") | Some(&"SUBJECT") => {
                *i += 2;
                *i <= t.len()
            }
            _ => false,
        }
    }

    #[test]
    fn imap_generic_search_syntax() {
        let mut s = Session::login(script(load(GENERIC), 7), "me@example.org", "pass word").unwrap();
        assert!(!s.is_gmail());
        let (mb, uv) = s.examine_all().unwrap();
        assert_eq!((mb.as_str(), uv), ("INBOX", 1700000000));
        let senders: Vec<String> = (0..25).map(|i| format!("s{}.example.com", i)).collect();
        let senders: Vec<&str> = senders.iter().map(|s| s.as_str()).collect();
        let uids = s.search(days_from_civil(2025, 11, 4) as i64, &senders, &["receipt", HEB], 0).unwrap();
        assert_eq!(uids, vec![12, 9, 5]); // A7 NO [BADCHARSET] ignored
        let o = out(&s);
        let lines: Vec<&str> = o.split("\r\n").collect();
        assert_eq!(lines[0], "A1 LOGIN \"me@example.org\" \"pass word\"");
        assert_eq!(lines[3], "A4 EXAMINE \"INBOX\"");
        for (l, n) in [(lines[4], 20), (lines[5], 6)] {
            let rest = l.split_once(" UID SEARCH SINCE 4-Nov-2025 ").unwrap().1;
            let toks: Vec<&str> = rest.split(' ').collect();
            let mut i = 0;
            assert!(key(&toks, &mut i) && i == toks.len(), "malformed: {}", l);
            assert_eq!(toks.iter().filter(|t| **t == "FROM" || **t == "SUBJECT").count(), n);
        }
        assert!(lines[4].starts_with("A5 UID SEARCH SINCE 4-Nov-2025 OR OR "));
        assert!(lines[5].ends_with("FROM \"s24.example.com\" SUBJECT \"receipt\""));
        assert_eq!(lines[6], "A7 UID SEARCH CHARSET UTF-8 SINCE 4-Nov-2025 SUBJECT {8}");
        assert_eq!(lines[7], HEB);
        assert_read_only(&o);
        s.logout();
    }

    #[test]
    fn imap_list_literal_and_escaped_name() {
        let input = b"* OK hi\r\nA1 OK in\r\nA2 OK caps\r\n* LIST (\\All) \"/\" {9}\r\nAll \"Mail\r\nA3 OK\r\n* OK [UIDVALIDITY 3] x\r\nA4 OK\r\n";
        let mut s = Session::login(script(input.to_vec(), 3), "u", "p").unwrap();
        let (mb, uv) = s.examine_all().unwrap();
        assert_eq!((mb.as_str(), uv), ("All \"Mail", 3));
        assert!(out(&s).ends_with("A4 EXAMINE \"All \\\"Mail\"\r\n"));
    }

    #[test]
    fn imap_login_errors() {
        assert!(matches!(Session::login(script(load(AUTH_FAIL), 1 << 20), "u@example.com", "x").err(), Some(ImapError::Auth(_))));
        let gm = |line: &str| Session::login(script(format!("* OK Gimap ready\r\nA1 NO {}\r\n", line).into_bytes(), 99), "u", "p").err();
        let app = gm("[ALERT] Application-specific password required: https://support.google.com/accounts/answer/185833 (Failure)");
        assert!(matches!(app, Some(ImapError::Auth(_))));
        let off = gm("[ALERT] Your account is not enabled for IMAP use. Please visit your Gmail settings page and enable your account for IMAP access. (Failure)");
        assert!(matches!(off, Some(ImapError::ImapOff)));
        assert!(matches!(gm("LOGIN failed."), Some(ImapError::Auth(_))));
        // CR/LF in credentials: rejected before anything is sent.
        let r = Session::login(script(b"* OK hi\r\n".to_vec(), 99), "u", "a\r\nA2 DELETE INBOX");
        assert!(matches!(r.err(), Some(ImapError::Auth(_))));
        // Non-ASCII password goes as a synchronizing literal after "+".
        let s = Session::login(script(b"* OK hi\r\n+ go\r\nA1 OK in\r\nA2 OK\r\n".to_vec(), 2), "u", "\u{043F}\u{0430}\u{0440}\u{043E}\u{043B}\u{044C}").unwrap();
        assert_eq!(out(&s), "A1 LOGIN \"u\" {12}\r\n\u{043F}\u{0430}\u{0440}\u{043E}\u{043B}\u{044C}\r\nA2 CAPABILITY\r\n");
        // Quotes and backslashes escaped.
        let s = Session::login(script(b"* OK hi\r\nA1 OK\r\nA2 OK\r\n".to_vec(), 99), "a\"b\\c", "x").unwrap();
        assert!(out(&s).starts_with("A1 LOGIN \"a\\\"b\\\\c\" \"x\"\r\n"));
        // PREAUTH: no LOGIN at all.
        let s = Session::login(script(b"* PREAUTH hi\r\nA1 OK\r\n".to_vec(), 99), "u", "p").unwrap();
        assert_eq!(out(&s), "A1 CAPABILITY\r\n");
    }

    #[test]
    fn imap_bye_mid_session() {
        let mut s = Session::login(script(load(BYE), 4), "u", "p").unwrap();
        assert!(matches!(s.examine_all(), Err(ImapError::Net(_))));
        // EOF in the middle of a literal.
        let mut s = Session::login(script(b"* OK\r\nA1 OK\r\nA2 OK\r\n* LIST (\\All) \"/\" {50}\r\nshort".to_vec(), 4), "u", "p").unwrap();
        assert!(matches!(s.examine_all(), Err(ImapError::Net(_))));
    }

    #[test]
    fn imap_cancel_between_batches() {
        let input = b"* OK\r\nA1 OK\r\nA2 OK\r\n* 1 FETCH (UID 30 BODY[TEXT]<0> {2}\r\nhi)\r\nA3 OK\r\n".to_vec();
        let mut s = Session::login(script(input, 6), "u", "p").unwrap();
        let uids: Vec<u32> = (1..=30).rev().collect();
        let cancel = AtomicBool::new(false);
        let mut n = 0;
        let r = s.fetch(&uids, &cancel, &mut |m| {
            assert_eq!(m.body, b"hi");
            n += 1;
            cancel.store(true, Ordering::Relaxed);
        });
        assert!(matches!(r, Err(ImapError::Cancelled)));
        assert_eq!(n, 1);
        let o = out(&s);
        assert_eq!(o.matches("UID FETCH").count(), 1);
        assert!(o.contains("A3 UID FETCH 30,29,"));
        assert!(o.contains(",6 (UID"));
    }

    #[test]
    fn imap_big_literal_truncated_in_sync() {
        let mut input = b"* OK\r\nA1 OK\r\nA2 OK\r\n* 1 FETCH (UID 9 BODY[HEADER.FIELDS (FROM)] {70000}\r\n".to_vec();
        input.extend(std::iter::repeat(b'x').take(70000));
        input.extend_from_slice(b" BODY[TEXT]<0> {3}\r\nend INTERNALDATE \"29-Feb-2024 00:00:00 +1400\")\r\nA3 OK done\r\n* 2 FETCH (UID 10 BODY[TEXT]<0> \"q\")\r\nA4 OK\r\n");
        let mut s = Session::login(script(input, 4096), "u", "p").unwrap();
        let mut got = Vec::new();
        s.fetch(&[9], &AtomicBool::new(false), &mut |m| got.push(m)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].header.len(), LIT_CAP);
        assert_eq!(got[0].body, b"end");
        assert_eq!(got[0].date, days_from_civil(2024, 2, 29));
        // The next answer still parses: the dropped bytes were consumed, not left in the stream.
        s.fetch(&[10], &AtomicBool::new(false), &mut |m| got.push(m)).unwrap();
        assert_eq!((got[1].uid, got[1].body.as_slice()), (10, &b"q"[..]));
    }

    #[test]
    fn imap_session_cap() {
        let mut input = b"* OK\r\nA1 OK\r\nA2 OK\r\n* 1 FETCH (UID 9 BODY[TEXT]<0> {99999999}\r\n".to_vec();
        input.extend(std::iter::repeat(b'x').take(SESSION_CAP + 10));
        let mut s = Session::login(script(input, 1 << 16), "u", "p").unwrap();
        assert!(matches!(s.fetch(&[9], &AtomicBool::new(false), &mut |_| {}), Err(ImapError::Proto(_))));
    }

    #[test]
    fn imap_gmail_query_chunks() {
        let senders: Vec<String> = (0..200).map(|i| format!("billing{}.example.com", i)).collect();
        let senders: Vec<&str> = senders.iter().map(|s| s.as_str()).collect();
        let q = gmail_queries("2025/01/02", &senders, &["receipt", "invoice"]);
        assert!(q.len() > 3);
        assert!(q.iter().all(|q| q.len() <= GM_MAX && q.starts_with("after:2025/01/02 ")));
        let all = q.join(" ");
        assert!(senders.iter().all(|s| all.contains(&format!("{} ", s)) || all.contains(&format!("{})", s))));
        assert!(q.last().unwrap().contains("subject:(receipt OR invoice)"));
        assert_eq!(parse_date(b" 4-Nov-2026 23:59:59 -0800"), Some(days_from_civil(2026, 11, 4)));
        assert_eq!(parse_date(b"junk"), None);
    }
}
