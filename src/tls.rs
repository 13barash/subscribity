//! TLS 1.2 client over a `TcpStream`, using SChannel through raw SSPI calls.
//! The OS checks the certificate chain and the host name (target name = host);
//! there is no switch to turn that off. Used for IMAP on port 993 (implicit TLS).

use std::ffi::c_void;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CRYPT_E_REVOKED, SEC_E_ALGORITHM_MISMATCH, SEC_E_CERT_EXPIRED, SEC_E_CERT_UNKNOWN, SEC_E_CERT_WRONG_USAGE,
    SEC_E_INCOMPLETE_MESSAGE, SEC_E_ISSUING_CA_UNTRUSTED, SEC_E_OK, SEC_E_UNTRUSTED_ROOT, SEC_E_WRONG_PRINCIPAL,
    SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED, SEC_I_INCOMPLETE_CREDENTIALS, SEC_I_RENEGOTIATE,
};
use windows_sys::Win32::Security::Authentication::Identity::*;
use windows_sys::Win32::Security::Credentials::SecHandle;

const REQ: u32 = ISC_REQ_SEQUENCE_DETECT
    | ISC_REQ_REPLAY_DETECT
    | ISC_REQ_CONFIDENTIALITY
    | ISC_REQ_ALLOCATE_MEMORY
    | ISC_REQ_STREAM
    | ISC_REQ_EXTENDED_ERROR;
/// Read size for ciphertext: one full TLS record (16 KB payload + overhead).
const CHUNK: usize = 16 * 1024 + 512;
/// Safety cap on ciphertext buffered while waiting for a complete record / handshake message.
const MAX_RAW: usize = 1 << 20;

pub struct Tls {
    sock: TcpStream,
    cred: SecHandle,
    ctx: SecHandle,
    has_cred: bool,
    has_ctx: bool,
    sizes: SecPkgContext_StreamSizes,
    raw: Vec<u8>,   // ciphertext received but not yet decrypted
    plain: Vec<u8>, // decrypted bytes not yet handed out
    pos: usize,
    eof: bool,
}

fn sb(ty: u32, len: usize, p: *mut u8) -> SecBuffer {
    SecBuffer { cbBuffer: len as u32, BufferType: ty, pvBuffer: p as *mut c_void }
}

fn sd(b: &mut [SecBuffer]) -> SecBufferDesc {
    SecBufferDesc { ulVersion: SECBUFFER_VERSION, cBuffers: b.len() as u32, pBuffers: b.as_mut_ptr() }
}

/// Short plain text for an SSPI status code.
fn sec_err(st: i32) -> String {
    let cert = (st as u32 >> 16) == 0x800B // FACILITY_CERT: CERT_E_* / TRUST_E_*
        || matches!(
            st,
            SEC_E_UNTRUSTED_ROOT
                | SEC_E_WRONG_PRINCIPAL
                | SEC_E_CERT_EXPIRED
                | SEC_E_CERT_UNKNOWN
                | SEC_E_CERT_WRONG_USAGE
                | SEC_E_ISSUING_CA_UNTRUSTED
                | CRYPT_E_REVOKED
        );
    if cert {
        "certificate check failed".into()
    } else if st == SEC_E_ALGORITHM_MISMATCH {
        "server does not support TLS 1.2".into()
    } else {
        format!("secure connection failed (0x{:08X})", st as u32)
    }
}

fn io_text(e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "connection timed out".into(),
        ErrorKind::InvalidData => e.to_string(),
        _ => "connection lost".into(),
    }
}

fn bad(msg: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg)
}

impl Tls {
    /// Connects (`timeout` for the TCP connect, then as read/write timeout) and runs the handshake.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Tls, String> {
        let addrs = (host, port).to_socket_addrs().map_err(|_| "server not found".to_string())?;
        let mut sock = None;
        let mut timed_out = false;
        for a in addrs {
            match TcpStream::connect_timeout(&a, timeout) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => timed_out |= matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock),
            }
        }
        let sock = sock.ok_or(if timed_out { "connection timed out" } else { "could not connect" })?;
        let _ = sock.set_read_timeout(Some(timeout));
        let _ = sock.set_write_timeout(Some(timeout));
        let _ = sock.set_nodelay(true);
        let zero = SecHandle { dwLower: 0, dwUpper: 0 };
        let mut t = Tls {
            sock,
            cred: zero,
            ctx: zero,
            has_cred: false,
            has_ctx: false,
            sizes: SecPkgContext_StreamSizes { cbHeader: 0, cbTrailer: 0, cbMaximumMessage: 0, cBuffers: 0, cbBlockSize: 0 },
            raw: Vec::new(),
            plain: Vec::new(),
            pos: 0,
            eof: false,
        };
        unsafe {
            let mut sc: SCHANNEL_CRED = std::mem::zeroed();
            sc.dwVersion = SCHANNEL_CRED_VERSION;
            sc.grbitEnabledProtocols = SP_PROT_TLS1_2_CLIENT;
            sc.dwFlags = SCH_CRED_AUTO_CRED_VALIDATION | SCH_CRED_NO_DEFAULT_CREDS | SCH_USE_STRONG_CRYPTO;
            let st = AcquireCredentialsHandleW(
                null(),
                UNISP_NAME_W,
                SECPKG_CRED_OUTBOUND,
                null(),
                &sc as *const SCHANNEL_CRED as *const c_void,
                None,
                null(),
                &mut t.cred,
                null_mut(),
            );
            if st != SEC_E_OK {
                return Err(sec_err(st));
            }
            t.has_cred = true;
        }
        t.handshake(host)?;
        Ok(t)
    }

    /// A second handle to the socket, so another thread can `shutdown` it to cancel.
    pub fn socket(&self) -> io::Result<TcpStream> {
        self.sock.try_clone()
    }

    /// Appends whatever the socket has (up to one record) to `raw`. Ok(0) = peer closed.
    fn fill_raw(&mut self) -> io::Result<usize> {
        if self.raw.len() > MAX_RAW {
            return Err(bad("secure connection failed (record too large)".into()));
        }
        let len = self.raw.len();
        self.raw.resize(len + CHUNK, 0);
        let r = loop {
            match self.sock.read(&mut self.raw[len..]) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        self.raw.truncate(len + *r.as_ref().unwrap_or(&0));
        r
    }

    /// Drops consumed input from `raw`, keeping only the SECBUFFER_EXTRA tail (if any).
    fn keep_extra(&mut self, extra: &SecBuffer) {
        let keep = if extra.BufferType == SECBUFFER_EXTRA { (extra.cbBuffer as usize).min(self.raw.len()) } else { 0 };
        let n = self.raw.len();
        self.raw.drain(..n - keep);
    }

    fn handshake(&mut self, host: &str) -> Result<(), String> {
        let target: Vec<u16> = host.encode_utf16().chain(Some(0)).collect();
        let mut first = true;
        let mut need_read = false;
        let mut cred_asks = 0;
        loop {
            if need_read {
                match self.fill_raw() {
                    Ok(0) => return Err("secure connection failed (closed by server)".into()),
                    Ok(_) => {}
                    Err(e) => return Err(io_text(&e)),
                }
            }
            let mut inb = [sb(SECBUFFER_TOKEN, self.raw.len(), self.raw.as_mut_ptr()), sb(SECBUFFER_EMPTY, 0, null_mut())];
            let ind = sd(&mut inb);
            let mut outb = [sb(SECBUFFER_TOKEN, 0, null_mut())];
            let mut outd = sd(&mut outb);
            let mut attr = 0u32;
            let ctx: *mut SecHandle = &mut self.ctx;
            let st = unsafe {
                InitializeSecurityContextW(
                    &self.cred,
                    if first { null() } else { ctx },
                    target.as_ptr(),
                    REQ,
                    0,
                    0,
                    if first { null() } else { &ind },
                    0,
                    ctx,
                    &mut outd,
                    &mut attr,
                    null_mut(),
                )
            };
            if first {
                if st < 0 {
                    return Err(sec_err(st));
                }
                self.has_ctx = true;
                first = false;
            }
            if outb[0].cbBuffer > 0 && !outb[0].pvBuffer.is_null() {
                let tok = unsafe { std::slice::from_raw_parts(outb[0].pvBuffer as *const u8, outb[0].cbBuffer as usize) };
                let sent = self.sock.write_all(tok);
                unsafe { FreeContextBuffer(outb[0].pvBuffer) };
                if st >= 0 {
                    sent.map_err(|e| io_text(&e))?;
                }
            }
            match st {
                SEC_E_OK | SEC_I_CONTINUE_NEEDED => {
                    self.keep_extra(&inb[1]);
                    if st == SEC_E_OK {
                        break;
                    }
                    need_read = self.raw.is_empty();
                }
                SEC_E_INCOMPLETE_MESSAGE => need_read = true,
                SEC_I_INCOMPLETE_CREDENTIALS => {
                    // The server asked for a client certificate; carry on without one.
                    cred_asks += 1;
                    if cred_asks > 2 {
                        return Err("server wants a client certificate".into());
                    }
                    need_read = false;
                }
                _ => return Err(sec_err(st)),
            }
        }
        // Whatever is left in `raw` is application data that came with the last handshake record.
        let st = unsafe {
            QueryContextAttributesW(&self.ctx, SECPKG_ATTR_STREAM_SIZES, &mut self.sizes as *mut SecPkgContext_StreamSizes as *mut c_void)
        };
        if st != SEC_E_OK {
            return Err(sec_err(st));
        }
        Ok(())
    }

    /// Decrypts one record from `raw`. Ok(false) = record incomplete, more bytes needed.
    fn decrypt(&mut self) -> io::Result<bool> {
        let p = self.raw.as_mut_ptr();
        let mut b = [
            sb(SECBUFFER_DATA, self.raw.len(), p),
            sb(SECBUFFER_EMPTY, 0, null_mut()),
            sb(SECBUFFER_EMPTY, 0, null_mut()),
            sb(SECBUFFER_EMPTY, 0, null_mut()),
        ];
        let d = sd(&mut b);
        let st = unsafe { DecryptMessage(&self.ctx, &d, 0, null_mut()) };
        match st {
            SEC_E_OK => {
                self.plain.clear();
                self.pos = 0;
                let mut extra = sb(SECBUFFER_EMPTY, 0, null_mut());
                for x in &b {
                    if x.BufferType == SECBUFFER_DATA && x.cbBuffer > 0 && !x.pvBuffer.is_null() {
                        // Decrypted in place: points into `raw`, which is untouched until keep_extra.
                        let s = unsafe { std::slice::from_raw_parts(x.pvBuffer as *const u8, x.cbBuffer as usize) };
                        self.plain.extend_from_slice(s);
                    } else if x.BufferType == SECBUFFER_EXTRA {
                        extra = *x;
                    }
                }
                self.keep_extra(&extra);
                Ok(true)
            }
            SEC_E_INCOMPLETE_MESSAGE => Ok(false),
            SEC_I_CONTEXT_EXPIRED => {
                // close_notify from the server: clean end of stream.
                self.eof = true;
                self.raw.clear();
                Ok(true)
            }
            SEC_I_RENEGOTIATE => Err(bad("secure connection failed (renegotiation refused)".into())),
            _ => Err(bad(sec_err(st))),
        }
    }
}

impl Read for Tls {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.plain.len() {
                let n = out.len().min(self.plain.len() - self.pos);
                out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            if !self.raw.is_empty() && self.decrypt()? {
                continue;
            }
            if self.fill_raw()? == 0 {
                // TCP closed without close_notify; the IMAP layer notices an incomplete answer.
                self.eof = true;
            }
        }
    }
}

impl Write for Tls {
    /// Encrypts up to cbMaximumMessage bytes into one record and sends it.
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let s = self.sizes;
        let n = data.len().min(s.cbMaximumMessage.max(1) as usize);
        let (h, t) = (s.cbHeader as usize, s.cbTrailer as usize);
        let mut m = vec![0u8; h + n + t];
        m[h..h + n].copy_from_slice(&data[..n]);
        let p = m.as_mut_ptr();
        let mut b = unsafe {
            [
                sb(SECBUFFER_STREAM_HEADER, h, p),
                sb(SECBUFFER_DATA, n, p.add(h)),
                sb(SECBUFFER_STREAM_TRAILER, t, p.add(h + n)),
                sb(SECBUFFER_EMPTY, 0, null_mut()),
            ]
        };
        let d = sd(&mut b);
        let st = unsafe { EncryptMessage(&self.ctx, 0, &d, 0) };
        if st != SEC_E_OK {
            return Err(bad(sec_err(st)));
        }
        let total = (b[0].cbBuffer + b[1].cbBuffer + b[2].cbBuffer) as usize;
        self.sock.write_all(&m[..total.min(m.len())])?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Tls {
    fn drop(&mut self) {
        unsafe {
            if self.has_ctx {
                DeleteSecurityContext(&self.ctx);
            }
            if self.has_cred {
                FreeCredentialsHandle(&self.cred);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(15);

    #[test]
    #[ignore]
    fn tls_gmail_greeting_and_command() {
        let mut t = Tls::connect("imap.gmail.com", 993, T).unwrap();
        let mut b = [0u8; 512];
        let n = t.read(&mut b).unwrap();
        assert!(b[..n].starts_with(b"* OK"), "greeting");
        t.write_all(b"a1 CAPABILITY\r\n").unwrap();
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"a1 OK") {
            let n = t.read(&mut b).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&b[..n]);
        }
        assert!(got.windows(10).any(|w| w == b"X-GM-EXT-1"));
        t.write_all(b"a2 LOGOUT\r\n").unwrap();
    }

    #[test]
    #[ignore]
    fn tls_http_roundtrip_to_eof() {
        // Several records per response + clean end of stream.
        let mut t = Tls::connect("badssl.com", 443, T).unwrap();
        t.write_all(b"GET / HTTP/1.1\r\nHost: badssl.com\r\nConnection: close\r\n\r\n").unwrap();
        let mut got = Vec::new();
        t.read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"HTTP/1.1 200"));
        assert!(got.len() > 2000);
    }

    #[test]
    #[ignore]
    fn tls_wrong_host_fails() {
        assert_eq!(Tls::connect("wrong.host.badssl.com", 443, T).err().unwrap(), "certificate check failed");
    }

    #[test]
    #[ignore]
    fn tls_expired_fails() {
        assert_eq!(Tls::connect("expired.badssl.com", 443, T).err().unwrap(), "certificate check failed");
    }

    #[test]
    #[ignore]
    fn tls_untrusted_root_fails() {
        assert_eq!(Tls::connect("untrusted-root.badssl.com", 443, T).err().unwrap(), "certificate check failed");
    }
}
