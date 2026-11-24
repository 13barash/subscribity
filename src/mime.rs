//! Mail parsing for the scanner: unfolded headers, RFC 2047 encoded words, quoted-printable and
//! base64 bodies (tolerant of truncation), one level of multipart plus a nested
//! multipart/alternative, charsets via MultiByteToWideChar, HTML to plain text.

/// One fetched message: raw header lines (FROM/SUBJECT/CONTENT-TYPE/CONTENT-TRANSFER-ENCODING)
/// and the raw body (BODY[TEXT], possibly truncated).
pub struct RawMail<'a> {
    pub header: &'a [u8],
    pub body: &'a [u8],
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub from_addr: String, // lowercase email
    pub from_name: String,
    pub subject: String,
    pub text: String, // plain text, whitespace-collapsed (single spaces, single newlines)
    pub alts: String, // short <img alt> texts of the HTML part (card brand logos), space-separated
}

const TEXT_CAP: usize = 16 * 1024;

pub fn parse(m: &RawMail) -> Parsed {
    let hs = unfold(m.header);
    let (from_name, from_addr) = split_from(&decode_words(field(&hs, "from")));
    let subject = collapse(&decode_words(field(&hs, "subject")));
    let ct = String::from_utf8_lossy(field(&hs, "content-type")).into_owned();
    let cte = String::from_utf8_lossy(field(&hs, "content-transfer-encoding")).into_owned();
    let (mut plain, mut html) = (None, None);
    collect(&ct, &cte, m.body, 0, &mut plain, &mut html);
    let alts = html.as_ref().map(|h: &(String, String)| h.1.clone()).unwrap_or_default();
    let mut text = match (plain, html.map(|h| h.0)) {
        (Some(p), _) if p.chars().count() >= 80 => p,
        (_, Some(h)) if !h.is_empty() => h,
        (p, _) => p.unwrap_or_default(),
    };
    if text.len() > TEXT_CAP {
        let mut i = TEXT_CAP;
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        text.truncate(i);
    }
    Parsed { from_addr, from_name, subject, text, alts }
}

// ---------- headers ----------

/// Header lines → (lowercase name, unfolded raw value).
fn unfold(h: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for line in h.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some(last) = out.last_mut() {
                last.1.push(b' ');
                last.1.extend_from_slice(line.trim_ascii());
            }
        } else if let Some(i) = line.iter().position(|&b| b == b':') {
            let name = String::from_utf8_lossy(&line[..i]).trim().to_ascii_lowercase();
            out.push((name, line[i + 1..].trim_ascii().to_vec()));
        }
    }
    out
}

fn field<'a>(hs: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    hs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice()).unwrap_or(&[])
}

/// RFC 2047: `=?charset?B|Q?text?=`. Whitespace between adjacent encoded words is dropped, and
/// adjacent words in the same charset are decoded together (a character may span two words).
fn decode_words(raw: &[u8]) -> String {
    let mut out = String::new();
    let (mut pend, mut pend_cs) = (Vec::new(), String::new());
    let (mut gap, mut i, mut last_word) = (0, 0, false);
    while i + 1 < raw.len() {
        if raw[i] == b'=' && raw[i + 1] == b'?' {
            if let Some((cs, bytes, end)) = encoded_word(raw, i) {
                let joined = last_word && raw[gap..i].iter().all(|b| b.is_ascii_whitespace());
                if !joined || cs != pend_cs {
                    out.push_str(&decode_charset(&pend, &pend_cs));
                    pend.clear();
                    if !joined {
                        out.push_str(&String::from_utf8_lossy(&raw[gap..i]));
                    }
                    pend_cs = cs;
                }
                pend.extend_from_slice(&bytes);
                last_word = true;
                i = end;
                gap = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&decode_charset(&pend, &pend_cs));
    out.push_str(&String::from_utf8_lossy(&raw[gap..]));
    out
}

/// Parses one encoded word at `i`: (lowercase charset, decoded bytes, end index).
fn encoded_word(raw: &[u8], i: usize) -> Option<(String, Vec<u8>, usize)> {
    let rest = &raw[i + 2..raw.len().min(i + 2 + 1000)];
    let q1 = rest.iter().position(|&b| b == b'?')?;
    let enc = *rest.get(q1 + 1)?;
    if rest.get(q1 + 2) != Some(&b'?') {
        return None;
    }
    let body = &rest[q1 + 3..];
    let end = body.windows(2).position(|w| w == b"?=")?;
    let text = &body[..end];
    if text.iter().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let cs = String::from_utf8_lossy(&rest[..q1]).to_ascii_lowercase();
    let cs = cs.split('*').next().unwrap_or("").to_string(); // RFC 2231 language suffix
    let bytes = match enc {
        b'B' | b'b' => base64(text),
        b'Q' | b'q' => qp(text, true),
        _ => return None,
    };
    Some((cs, bytes, i + 2 + q1 + 3 + end + 2))
}

/// `"Name" <addr>` / `addr (Name)` / `addr` → (name, lowercase addr).
fn split_from(s: &str) -> (String, String) {
    let (name, addr) = if let Some(lt) = s.rfind('<') {
        let rest = &s[lt + 1..];
        (s[..lt].to_string(), rest[..rest.find('>').unwrap_or(rest.len())].to_string())
    } else {
        let addr = s.split_whitespace().find(|w| w.contains('@')).unwrap_or("").to_string();
        let name = match (s.find('('), s.rfind(')')) {
            (Some(a), Some(b)) if a < b => s[a + 1..b].to_string(),
            _ => String::new(),
        };
        (name, addr)
    };
    let name = collapse(name.trim().trim_matches('"').trim());
    (name, addr.trim().trim_matches(|c| c == '"' || c == '(' || c == ')').to_ascii_lowercase())
}

/// Value of parameter `key` in a header like `text/plain; charset="utf-8"`.
fn param(h: &str, key: &str) -> Option<String> {
    let mut rest = h.split_once(';')?.1;
    loop {
        let t = rest.trim_start_matches(|c: char| c == ';' || c.is_whitespace());
        let eq = t.find('=')?;
        let k = t[..eq].trim().to_ascii_lowercase();
        let v = t[eq + 1..].trim_start();
        let (val, next) = if let Some(q) = v.strip_prefix('"') {
            let e = q.find('"').unwrap_or(q.len());
            (q[..e].to_string(), &q[(e + 1).min(q.len())..])
        } else {
            let e = v.find(';').unwrap_or(v.len());
            (v[..e].trim().to_string(), &v[e..])
        };
        if k == key {
            return Some(val);
        }
        rest = next;
    }
}

fn mime_type(ct: &str) -> String {
    ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

// ---------- bodies ----------

/// Walks the body: first text/plain and first text/html found (top level, one multipart level,
/// and one nested multipart such as multipart/alternative inside multipart/mixed).
fn collect(ct: &str, cte: &str, body: &[u8], depth: u8, plain: &mut Option<String>, html: &mut Option<(String, String)>) {
    let mt = mime_type(ct);
    if mt.starts_with("multipart/") {
        if let Some(b) = param(ct, "boundary").filter(|b| !b.is_empty()) {
            if depth < 2 {
                for part in split_parts(body, b.as_bytes()) {
                    let (h, b) = split_header(part);
                    let hs = unfold(h);
                    let disp = String::from_utf8_lossy(field(&hs, "content-disposition")).to_ascii_lowercase();
                    if disp.starts_with("attachment") {
                        continue;
                    }
                    let pct = String::from_utf8_lossy(field(&hs, "content-type")).into_owned();
                    let pcte = String::from_utf8_lossy(field(&hs, "content-transfer-encoding")).into_owned();
                    collect(&pct, &pcte, b, depth + 1, plain, html);
                }
            }
            return;
        }
    }
    let is_html = mt == "text/html";
    if !(is_html || mt.is_empty() || mt.starts_with("text/")) {
        return;
    }
    if (is_html && html.is_some()) || (!is_html && plain.is_some()) {
        return;
    }
    let raw = match cte.trim().to_ascii_lowercase().as_str() {
        "quoted-printable" => qp(body, false),
        "base64" => base64(body),
        _ => body.to_vec(),
    };
    let s = decode_charset(&raw, &param(ct, "charset").unwrap_or_default());
    if is_html {
        *html = Some((html_to_text(&s), img_alts(&s)));
    } else {
        *plain = Some(collapse(&s));
    }
}

/// Part bodies between `--boundary` lines. A missing closing boundary (truncated mail) keeps the
/// last part.
fn split_parts<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let (mut start, mut pos) = (None, 0);
    while pos <= body.len() {
        let end = body[pos..].iter().position(|&b| b == b'\n').map_or(body.len(), |e| pos + e);
        let line = &body[pos..end];
        if line.len() >= boundary.len() + 2 && line.starts_with(b"--") && line[2..].starts_with(boundary) {
            if let Some(s) = start {
                parts.push(&body[s..pos]);
            }
            if line[2 + boundary.len()..].starts_with(b"--") {
                return parts;
            }
            start = Some((end + 1).min(body.len()));
        }
        pos = end + 1;
    }
    if let Some(s) = start {
        parts.push(&body[s..]);
    }
    parts
}

fn split_header(part: &[u8]) -> (&[u8], &[u8]) {
    for (i, w) in part.windows(2).enumerate() {
        if w == b"\n\n" {
            return (&part[..i], &part[i + 2..]);
        }
        if w == b"\n\r" && part.get(i + 2) == Some(&b'\n') {
            return (&part[..i], &part[i + 3..]);
        }
    }
    (part, &[])
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|v| v as u8)
}

/// Quoted-printable (`header` = Q encoding: `_` is a space). A cut-off `=X` at the end is dropped.
fn qp(b: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'=' {
            match (b.get(i + 1).copied(), b.get(i + 2).copied()) {
                (Some(b'\r'), Some(b'\n')) => i += 3,
                (Some(b'\n'), _) => i += 2,
                (Some(h), Some(l)) if hex(h).is_some() && hex(l).is_some() => {
                    out.push(hex(h).unwrap_or(0) << 4 | hex(l).unwrap_or(0));
                    i += 3;
                }
                (_, None) => break,
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        } else {
            out.push(if header && c == b'_' { b' ' } else { c });
            i += 1;
        }
    }
    out
}

/// Base64, skipping anything outside the alphabet; a cut-off final group yields its whole bytes.
fn base64(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    let (mut acc, mut n) = (0u32, 0u8);
    let flush = |out: &mut Vec<u8>, acc: u32, n: u8| match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    };
    for &c in b {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                flush(&mut out, acc, n);
                acc = 0;
                n = 0;
                continue;
            }
            _ => continue,
        };
        acc = acc << 6 | v as u32;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            n = 0;
        }
    }
    flush(&mut out, acc, n);
    out
}

// ---------- charsets ----------

fn codepage(label: &str) -> Option<u32> {
    let l = label.trim().trim_matches('"').to_ascii_lowercase();
    Some(match l.as_str() {
        "utf-8" | "utf8" => 65001,
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "us-ascii" | "ascii" => 1252,
        "iso-8859-2" | "iso8859-2" | "latin2" => 28592,
        "iso-8859-8" | "iso-8859-8-i" | "iso-8859-8-e" | "iso8859-8" => 28598,
        "iso-8859-15" | "iso8859-15" | "latin-9" => 28605,
        "koi8-r" => 20866,
        _ => {
            let n: u32 = l.strip_prefix("windows-").or_else(|| l.strip_prefix("cp"))?.parse().ok()?;
            if (1250..=1258).contains(&n) { n } else { return None }
        }
    })
}

/// Bytes in `label` → String. ASCII, UTF-8, unknown labels and conversion failures use lossy
/// UTF-8; a us-ascii/latin-1 label on valid UTF-8 bytes (a common mislabel) stays UTF-8.
pub fn decode_charset(b: &[u8], label: &str) -> String {
    if b.is_ascii() {
        return String::from_utf8_lossy(b).into_owned();
    }
    match codepage(label) {
        Some(1252) if std::str::from_utf8(b).is_ok() && !label.to_ascii_lowercase().starts_with("windows") => {
            String::from_utf8_lossy(b).into_owned()
        }
        Some(cp) if cp != 65001 => wide(b, cp).unwrap_or_else(|| String::from_utf8_lossy(b).into_owned()),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

fn wide(b: &[u8], cp: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    let len = b.len().min(1 << 24) as i32;
    // SAFETY: lengths passed match the buffers; the first call only measures.
    unsafe {
        let n = MultiByteToWideChar(cp, 0, b.as_ptr(), len, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut w = vec![0u16; n as usize];
        let n = MultiByteToWideChar(cp, 0, b.as_ptr(), len, w.as_mut_ptr(), n);
        (n > 0).then(|| String::from_utf16_lossy(&w[..n as usize]))
    }
}

// ---------- text ----------

/// Single spaces inside lines, single newlines between them; invisible format characters
/// (zero-width, bidi marks, soft hyphen) removed; NBSP and other Unicode spaces become spaces.
pub fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = 0u8;
    for c in s.chars() {
        if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
            ws = 2;
        } else if c.is_whitespace() {
            ws = ws.max(1);
        } else if matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{feff}' | '\u{ad}' | '\u{34f}') {
            continue;
        } else {
            if !out.is_empty() {
                match ws {
                    1 => out.push(' '),
                    2 => out.push('\n'),
                    _ => {}
                }
            }
            ws = 0;
            out.push(c);
        }
    }
    out
}

const BLOCK: [&str; 26] = [
    "p", "div", "br", "tr", "li", "ul", "ol", "table", "h1", "h2", "h3", "h4", "h5", "h6", "hr",
    "section", "article", "header", "footer", "blockquote", "center", "dt", "dd", "title", "tbody", "thead",
];
const ENTITIES: [(&str, &str); 26] = [
    ("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'"), ("nbsp", "\u{a0}"),
    ("euro", "€"), ("pound", "£"), ("yen", "¥"), ("cent", "¢"), ("copy", "©"), ("reg", "®"),
    ("trade", "™"), ("ndash", "–"), ("mdash", "—"), ("hellip", "…"), ("rsquo", "’"), ("lsquo", "‘"),
    ("rdquo", "”"), ("ldquo", "“"), ("middot", "·"), ("bull", "•"), ("laquo", "«"), ("raquo", "»"),
    ("zwnj", ""), ("shy", ""),
];

fn find(h: &[u8], pat: &[u8], from: usize) -> Option<usize> {
    h.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

/// Short (≤ 30 chars) `alt` texts of `<img>` tags, space-separated, max 1 KB: receipts often show
/// the card brand only as a logo ("Mastercard" image next to "- 1234").
pub fn img_alts(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::new();
    let mut f = 0;
    while let Some(p) = find(lower.as_bytes(), b"<img", f) {
        let end = find(lower.as_bytes(), b">", p).unwrap_or(lower.len());
        f = end;
        let Some(a) = lower[p..end].find("alt=").map(|a| p + a + 4) else { continue };
        let q = s.as_bytes().get(a).copied().filter(|q| *q == b'"' || *q == b'\'');
        let (from, stop) = match q {
            Some(q) => (a + 1, s[a + 1..end].find(q as char).map_or(end, |i| a + 1 + i)),
            None => (a, s[a..end].find(|c: char| c.is_whitespace() || c == '/').map_or(end, |i| a + i)),
        };
        let v = s.get(from..stop).unwrap_or("").trim();
        if !v.is_empty() && v.chars().count() <= 30 && out.len() + v.len() < 1024 {
            out.push_str(v);
            out.push(' ');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Visible text of an HTML document: head/style/script/comments dropped, block tags → newline,
/// table cells → space, entities decoded, whitespace collapsed.
pub fn html_to_text(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let (b, lb) = (s.as_bytes(), lower.as_bytes());
    let mut out = String::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => {
                if lb[i..].starts_with(b"<!--") {
                    i = find(lb, b"-->", i + 4).map_or(b.len(), |p| p + 3);
                    continue;
                }
                let close = b.get(i + 1) == Some(&b'/');
                let ns = i + 1 + close as usize;
                let mut j = ns;
                while j < b.len() && b[j].is_ascii_alphanumeric() {
                    j += 1;
                }
                if j == ns && b.get(ns) != Some(&b'!') {
                    out.push('<');
                    i += 1;
                    continue;
                }
                let name = &lower[ns..j];
                let mut end = j;
                // Quotes only open after `=` (attribute values), so a stray apostrophe can't eat the text.
                let (mut quote, mut prev) = (0u8, 0u8);
                while end < b.len() && !(quote == 0 && b[end] == b'>') {
                    match b[end] {
                        q @ (b'"' | b'\'') if quote == 0 && prev == b'=' => quote = q,
                        q if q == quote => quote = 0,
                        _ => {}
                    }
                    if !b[end].is_ascii_whitespace() {
                        prev = b[end];
                    }
                    end += 1;
                }
                i = (end + 1).min(b.len());
                if !close && matches!(name, "head" | "style" | "script" | "noscript") {
                    let pat = format!("</{}", name);
                    i = find(lb, pat.as_bytes(), i)
                        .and_then(|p| find(lb, b">", p))
                        .map_or(b.len(), |p| p + 1);
                } else if BLOCK.contains(&name) {
                    out.push('\n');
                } else if name == "td" || name == "th" {
                    out.push(' ');
                }
            }
            b'&' => {
                let (rep, n) = entity(&s[i..]);
                out.push_str(&rep);
                i += n;
            }
            _ => {
                let next = b[i..].iter().position(|&c| c == b'<' || c == b'&').map_or(b.len(), |p| p + i);
                out.push_str(&s[i..next]);
                i = next;
            }
        }
    }
    collapse(&out)
}

/// `s` starts with '&': (replacement, bytes consumed). Unknown entities stay literal.
fn entity(s: &str) -> (String, usize) {
    let semi = match s.as_bytes().iter().take(12).position(|&c| c == b';') {
        Some(p) if p > 1 => p,
        _ => return ("&".into(), 1),
    };
    let name = &s[1..semi];
    let code = if let Some(h) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        u32::from_str_radix(h, 16).ok()
    } else if let Some(d) = name.strip_prefix('#') {
        d.parse().ok()
    } else {
        return match ENTITIES.iter().find(|(n, _)| *n == name) {
            Some((_, r)) => (r.to_string(), semi + 1),
            None => ("&".into(), 1),
        };
    };
    match code.and_then(char::from_u32) {
        Some(c) => (c.to_string(), semi + 1),
        None => ("&".into(), 1),
    }
}

#[cfg(test)]
pub fn split_eml(eml: &[u8]) -> (&[u8], &[u8]) {
    split_header(eml)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_eml(eml: &[u8]) -> Parsed {
        let (header, body) = split_eml(eml);
        parse(&RawMail { header, body })
    }

    #[test]
    fn mime_encoded_words() {
        assert_eq!(decode_words(b"=?UTF-8?B?w6l0w6k=?= =?UTF-8?Q?_caf=C3=A9?="), "été café");
        assert_eq!(decode_words(b"Re: =?utf-8?q?hello?= world"), "Re: hello world");
        // A UTF-8 character split across two adjacent words.
        assert_eq!(decode_words(b"=?utf-8?B?4oI=?=\r\n =?utf-8?B?qg==?="), "₪");
        assert_eq!(decode_words(b"=?bogus"), "=?bogus");
    }

    #[test]
    fn mime_from_forms() {
        assert_eq!(split_from("\"Netflix\" <Info@Account.Netflix.com>"), ("Netflix".into(), "info@account.netflix.com".into()));
        assert_eq!(split_from("billing@acme.com (Acme)"), ("Acme".into(), "billing@acme.com".into()));
        assert_eq!(split_from("billing@acme.com"), ("".into(), "billing@acme.com".into()));
    }

    #[test]
    fn mime_qp_base64_truncation() {
        assert_eq!(qp(b"a=3Db=\r\nc=E2=82=AA=4", false), "a=bc₪".as_bytes());
        assert_eq!(base64(b"aGVsbG8gd29y"), b"hello wor");
        assert_eq!(base64(b"aGVsbG8gd29ybA"), b"hello worl");
        assert_eq!(base64(b"aGVs\r\nbG8="), b"hello");
    }

    #[test]
    fn mime_html_to_text() {
        let h = "<html><head><title>x</title><style>p{a:b}</style></head><body><!-- c --><p>Total&nbsp;&euro;9,99</p>\
                 <script>var a='<p>';</script><table><tr><td>A</td><td>B &amp; C &#8362;&#x20AC;</td></tr></table>a<b>b</b> &bogus; 1 < 2</body>";
        assert_eq!(html_to_text(h), "Total €9,99\nA B & C ₪€\nab &bogus; 1 < 2");
    }

    #[test]
    fn mime_params() {
        let ct = "multipart/alternative; boundary=\"b1;x\"; charset=UTF-8";
        assert_eq!(param(ct, "boundary").as_deref(), Some("b1;x"));
        assert_eq!(param(ct, "charset").as_deref(), Some("UTF-8"));
        assert_eq!(param("text/plain", "charset"), None);
        assert_eq!(mime_type(ct), "multipart/alternative");
    }

    #[test]
    fn mime_fixture_netflix_html_qp() {
        let p = parse_eml(include_bytes!("fixtures/netflix_qp.eml"));
        assert_eq!(p.from_addr, "info@account.netflix.com");
        assert_eq!(p.from_name, "Netflix");
        assert!(p.text.contains("Total ₪54.90"), "{}", p.text);
        assert!(!p.text.contains("font-family") && !p.text.contains('='));
    }

    #[test]
    fn mime_fixture_spotify_multipart_base64() {
        let p = parse_eml(include_bytes!("fixtures/spotify_b64.eml"));
        assert!(p.text.starts_with("Thanks for your payment"), "{}", p.text);
        assert!(!p.text.contains("<td"));
    }

    #[test]
    fn mime_fixture_charsets() {
        let p = parse_eml(include_bytes!("fixtures/win1252.eml"));
        assert!(p.text.contains("€4,99") && p.text.contains("Grüße"), "{}", p.text);
        let p = parse_eml(include_bytes!("fixtures/iso8859_8_subject.eml"));
        assert_eq!(p.subject, "קבלה על תשלום חודשי");
        assert_eq!(p.from_name, "הוט");
    }

    #[test]
    fn mime_fixture_truncated_base64() {
        let p = parse_eml(include_bytes!("fixtures/truncated_b64.eml"));
        assert!(p.text.contains("Total charged: $11.99"), "{}", p.text);
    }
}
