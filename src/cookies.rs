use aes::Aes128;
use anyhow::{anyhow, bail, Context, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use rusqlite::{Connection, OpenFlags};
use sha1::Sha1;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::browser::Browser;
use crate::keychain;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

pub struct ClaudeCookies {
    pub browser: Browser,
    pub last_active_org: String,
    pub all: HashMap<String, String>,
}

pub fn find_all_claude_sessions() -> Result<Vec<ClaudeCookies>> {
    let mut sessions: Vec<ClaudeCookies> = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for &browser in Browser::ALL {
        let root = match browser.profile_root()? {
            Some(r) => r,
            None => continue,
        };
        tried.push(browser.display_name().to_string());

        match try_browser(browser, &root) {
            Ok(Some(cookies)) => sessions.push(cookies),
            Ok(None) => {}
            Err(e) => errors.push(format!("{}: {e:#}", browser.display_name())),
        }
    }

    if !sessions.is_empty() {
        return Ok(sessions);
    }
    if tried.is_empty() {
        bail!(
            "no supported browser found. claude-meter supports Chrome, Arc, Brave, Edge on macOS."
        );
    }
    if errors.is_empty() {
        bail!(
            "no {} profile has claude.ai cookies. Log into claude.ai in one of them, then retry.",
            tried.join("/")
        );
    }
    bail!(
        "could not find claude.ai cookies in any installed browser ({}). Errors: {}",
        tried.join(", "),
        errors.join("; ")
    )
}

fn try_browser(browser: Browser, root: &Path) -> Result<Option<ClaudeCookies>> {
    let mut candidates: Vec<PathBuf> = vec![root.join("Default")];
    for entry in std::fs::read_dir(root)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if p.is_dir() && name.starts_with("Profile ") {
            candidates.push(p);
        }
    }

    // Find a profile with claude.ai cookies before we pay the Keychain cost.
    let mut cookies_path: Option<PathBuf> = None;
    'outer: for profile in &candidates {
        for sub in &["Network/Cookies", "Cookies"] {
            let p = profile.join(sub);
            if p.exists() && profile_has_claude(&p).unwrap_or(false) {
                cookies_path = Some(p);
                break 'outer;
            }
        }
    }
    let cookies_path = match cookies_path {
        Some(x) => x,
        None => return Ok(None),
    };

    let pw = keychain::safe_storage_password(browser).with_context(|| {
        format!(
            "read {} Safe Storage password from Keychain",
            browser.display_name()
        )
    })?;
    let key = derive_key(&pw);

    let temp_path = std::env::temp_dir().join(format!(
        "claude-meter-cookies-{}-{}.db",
        browser.display_name().to_ascii_lowercase(),
        std::process::id()
    ));
    std::fs::copy(&cookies_path, &temp_path)
        .with_context(|| format!("copy {}", cookies_path.display()))?;

    let decrypted = decrypt_all(&temp_path, &key);
    let _ = std::fs::remove_file(&temp_path);
    let decrypted = decrypted?;

    if !decrypted.contains_key("sessionKey") {
        bail!(
            "found claude.ai cookies in {} but no sessionKey, log into claude.ai again",
            browser.display_name()
        );
    }
    let last_active_org = decrypted
        .get("lastActiveOrg")
        .ok_or_else(|| anyhow!("no lastActiveOrg cookie found"))?
        .clone();

    Ok(Some(ClaudeCookies {
        browser,
        last_active_org,
        all: decrypted,
    }))
}

fn decrypt_all(db_path: &Path, key: &[u8; 16]) -> Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT name, encrypted_value FROM cookies WHERE host_key LIKE '%claude.ai%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut decrypted: HashMap<String, String> = HashMap::new();
    for row in rows {
        let (name, enc) = row?;
        if let Some(value) = decrypt_cookie(key, &enc) {
            decrypted.insert(name, value);
        }
    }
    Ok(decrypted)
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
