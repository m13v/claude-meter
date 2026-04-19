use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use rusqlite::{Connection, OpenFlags};
use sha1::Sha1;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

type Aes128CbcDec = cbc::Decryptor<Aes128>;

pub struct ClaudeCookies {
    pub last_active_org: String,
    pub all: HashMap<String, String>,
}

pub fn find_and_decrypt_claude_cookies(safe_storage_pw: &[u8]) -> Result<ClaudeCookies> {
    let key = derive_key(safe_storage_pw);
    let (profile_path, cookies_path) = find_profile_with_claude()?;

    // Copy the DB so we don't fight Chrome for the file lock.
    let temp_path = std::env::temp_dir()
        .join(format!("claude-meter-cookies-{}.db", std::process::id()));
    std::fs::copy(&cookies_path, &temp_path)
        .with_context(|| format!("copy {}", cookies_path.display()))?;

    let conn = Connection::open_with_flags(
        &temp_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut stmt = conn.prepare(
        "SELECT name, encrypted_value FROM cookies WHERE host_key LIKE '%claude.ai%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;

    let mut decrypted: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (name, enc) = row?;
        if let Some(value) = decrypt_cookie(&key, &enc) {
            decrypted.insert(name, value);
        }
    }
    drop(stmt);
    drop(conn);
    let _ = std::fs::remove_file(&temp_path);

    if !decrypted.contains_key("sessionKey") {
        bail!(
            "no sessionKey cookie found. Log into claude.ai in Chrome ({}), then retry.",
            profile_path.display()
        );
    }
    let last_active_org = decrypted
        .get("lastActiveOrg")
        .ok_or_else(|| anyhow!("no lastActiveOrg cookie found"))?
        .clone();

    Ok(ClaudeCookies {
        last_active_org,
        all: decrypted,
    })
}

fn derive_key(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<Sha1>(password, b"saltysalt", 1003, &mut key);
    key
}

fn decrypt_cookie(key: &[u8; 16], enc: &[u8]) -> Option<String> {
    if enc.len() < 3 {
        return None;
    }
    let ciphertext = if &enc[..3] == b"v10" || &enc[..3] == b"v11" {
        &enc[3..]
    } else {
        enc
    };
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return None;
    }
    let iv = [b' '; 16];
    let mut buf = ciphertext.to_vec();
    let plaintext = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .ok()?;

    // Chrome v20+ (Oct 2024+) prepends SHA256(host_key) = 32 bytes of opaque binary
    // to the cookie plaintext. Strip it if present. Heuristic: if the first byte
    // is non-ASCII and byte 32 is ASCII, we've got a prefix.
    let bytes: &[u8] = if plaintext.len() > 32
        && !is_printable(plaintext[0])
        && is_printable(plaintext[32])
    {
        &plaintext[32..]
    } else {
        plaintext
    };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn is_printable(b: u8) -> bool {
    (32..127).contains(&b)
}

fn find_profile_with_claude() -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let chrome_root = home.join("Library/Application Support/Google/Chrome");
    if !chrome_root.exists() {
        bail!(
            "Chrome not found at {}. claude-meter v0.1 requires Google Chrome on macOS.",
            chrome_root.display()
        );
    }

    let mut candidates: Vec<PathBuf> = vec![chrome_root.join("Default")];
    for entry in std::fs::read_dir(&chrome_root)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if p.is_dir() && name.starts_with("Profile ") {
            candidates.push(p);
        }
    }

    for profile in &candidates {
        for sub in &["Network/Cookies", "Cookies"] {
            let cookies = profile.join(sub);
            if cookies.exists() && profile_has_claude(&cookies).unwrap_or(false) {
                return Ok((profile.clone(), cookies));
            }
        }
    }
    bail!(
        "no Chrome profile has claude.ai cookies. Log into claude.ai in Chrome, then retry."
    )
}

fn profile_has_claude(cookies_db: &Path) -> Result<bool> {
    let uri = format!("file:{}?mode=ro&immutable=1", cookies_db.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cookies WHERE host_key LIKE '%claude.ai%'",
        [],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}
