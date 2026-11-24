// Self-update from GitHub Releases.
//
// A release must contain two assets: `Subscribity.exe` and `Subscribity.exe.sha256`
// (the hex SHA-256 of the exe). The release workflow in .github/workflows creates both.
// The download is only installed when its hash matches.
use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Networking::WinHttp::*;

pub const REPO: &str = "13barash/subscribity";
const EXE_ASSET: &str = "Subscribity.exe";

pub struct Release {
    pub version: String,
    pub exe_url: String,
    pub sha_url: String,
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// GET an https URL and return the body. Redirects (GitHub asset downloads) are followed.
pub fn get(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("only https is allowed")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    unsafe {
        let ses = WinHttpOpen(w("Subscribity-updater").as_ptr(), 4 /* automatic proxy */, null(), null(), 0);
        if ses.is_null() {
            return Err("network unavailable".into());
        }
        let con = WinHttpConnect(ses, w(host).as_ptr(), 443, 0);
        let req = if con.is_null() { null_mut() } else {
            WinHttpOpenRequest(con, w("GET").as_ptr(), w(path).as_ptr(), null(), null(), null(), 0x0080_0000 /* secure */)
        };
        let mut result = Err("could not connect".to_string());
        if !req.is_null() {
            let headers = w("Accept: application/vnd.github+json, application/octet-stream\r\n");
            if WinHttpSendRequest(req, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) != 0 && WinHttpReceiveResponse(req, null_mut()) != 0 {
                let mut status = 0u32;
                let mut size = 4u32;
                WinHttpQueryHeaders(req, 19 | 0x2000_0000 /* STATUS_CODE | FLAG_NUMBER */, null(), &mut status as *mut u32 as *mut c_void, &mut size, null_mut());
                if status != 200 {
                    result = Err(format!("server answered {}", status));
                } else {
                    let mut body = Vec::new();
                    loop {
                        let mut avail = 0u32;
                        if WinHttpQueryDataAvailable(req, &mut avail) == 0 {
                            result = Err("download interrupted".into());
                            break;
                        }
                        if avail == 0 {
                            result = Ok(std::mem::take(&mut body));
                            break;
                        }
                        if body.len() + avail as usize > limit {
                            result = Err("download too large".into());
                            break;
                        }
                        let mut buf = vec![0u8; avail as usize];
                        let mut read = 0u32;
                        if WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, avail, &mut read) == 0 {
                            result = Err("download interrupted".into());
                            break;
                        }
                        body.extend_from_slice(&buf[..read as usize]);
                    }
                }
            }
            WinHttpCloseHandle(req);
        }
        if !con.is_null() {
            WinHttpCloseHandle(con);
        }
        WinHttpCloseHandle(ses);
        result
    }
}

/// "v1.2.3" -> (1, 2, 3)
pub fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut p = v.trim().trim_start_matches(['v', 'V']).split(['.', '-']).map(|x| x.parse::<u32>().unwrap_or(0));
    (p.next().unwrap_or(0), p.next().unwrap_or(0), p.next().unwrap_or(0))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    !candidate.is_empty() && parse_version(candidate) > parse_version(current)
}

pub fn latest() -> Result<Release, String> {
    let body = match get(&format!("https://api.github.com/repos/{}/releases/latest", REPO), 2 << 20) {
        // No release published yet: nothing newer than this build.
        Err(e) if e.ends_with("404") => {
            return Ok(Release { version: String::new(), exe_url: String::new(), sha_url: String::new() });
        }
        r => r?,
    };
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| "unexpected answer from GitHub")?;
    let version = v["tag_name"].as_str().ok_or("no release found")?.trim_start_matches(['v', 'V']).to_string();
    let mut exe_url = String::new();
    let mut sha_url = String::new();
    for a in v["assets"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let name = a["name"].as_str().unwrap_or("");
        let url = a["browser_download_url"].as_str().unwrap_or("").to_string();
        if name.eq_ignore_ascii_case(EXE_ASSET) {
            exe_url = url;
        } else if name.eq_ignore_ascii_case(&format!("{}.sha256", EXE_ASSET)) {
            sha_url = url;
        }
    }
    Ok(Release { version, exe_url, sha_url })
}

pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::BCryptHash;
    let mut out = [0u8; 32];
    // 0x41 = BCRYPT_SHA256_ALG_HANDLE (pseudo-handle, Windows 10+)
    let st = unsafe { BCryptHash(0x41 as _, null(), 0, data.as_ptr(), data.len() as u32, out.as_mut_ptr(), 32) };
    if st != 0 {
        return Err("could not check the download".into());
    }
    Ok(out.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Downloads the new exe and checks its SHA-256 against the published one.
pub fn download_verified(rel: &Release) -> Result<Vec<u8>, String> {
    if rel.exe_url.is_empty() || rel.sha_url.is_empty() {
        return Err("this release has no Windows download yet".into());
    }
    let expected = String::from_utf8(get(&rel.sha_url, 4096)?).map_err(|_| "bad checksum file")?;
    let expected = expected.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let exe = get(&rel.exe_url, 64 << 20)?;
    if exe.len() < 64 * 1024 || !exe.starts_with(b"MZ") {
        return Err("the download is not a valid program".into());
    }
    if sha256_hex(&exe)? != expected {
        return Err("the download did not match its checksum, so it was not installed".into());
    }
    Ok(exe)
}

/// Replaces the running exe: Windows allows renaming a running exe, so the current one
/// becomes `.old` (deleted on next start) and the new one takes its name.
pub fn swap_in(exe_path: &Path, new_exe: &[u8]) -> Result<(), String> {
    let new_path = exe_path.with_extension("exe.new");
    let old_path = exe_path.with_extension("exe.old");
    std::fs::write(&new_path, new_exe).map_err(|e| format!("could not save the update: {}", e))?;
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(exe_path, &old_path).map_err(|e| format!("could not replace the app: {}", e))?;
    if let Err(e) = std::fs::rename(&new_path, exe_path) {
        let _ = std::fs::rename(&old_path, exe_path); // put the old version back
        return Err(format!("could not replace the app: {}", e));
    }
    Ok(())
}

pub fn cleanup_old(exe_path: &Path) {
    let _ = std::fs::remove_file(exe_path.with_extension("exe.old"));
    let _ = std::fs::remove_file(exe_path.with_extension("exe.new"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn sha256() {
        assert_eq!(sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
