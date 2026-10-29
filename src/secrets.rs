//! Private key store. API keys can come from the shell environment or from
//! `<AIWS_HOME>/secrets.env`, which `aiws key set` / the TUI write for you, so no
//! shell setup is needed. The file is plain text, restricted to your user account
//! (0600 on Unix, owner-only ACL on Windows). Keys are never put in config.toml,
//! the request log or any message.

use crate::config::{home_dir, is_env_name};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum KeySource {
    Shell,
    Stored,
    None,
}

pub fn path() -> PathBuf {
    home_dir().join("secrets.env")
}

fn read_all() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path()) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                if is_env_name(k.trim()) && !v.is_empty() {
                    m.insert(k.trim().to_string(), v.to_string());
                }
            }
        }
    }
    m
}

fn write_all(m: &BTreeMap<String, String>) -> Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut body = String::from("# aiws private keys. Do not share or commit this file.\n");
    for (k, v) in m {
        body.push_str(&format!("{k}={v}\n"));
    }
    let tmp = p.with_extension("env.tmp");
    std::fs::write(&tmp, body)?;
    restrict(&tmp);
    std::fs::rename(&tmp, &p)?;
    restrict(&p);
    Ok(())
}

#[cfg(unix)]
fn restrict(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(windows)]
fn restrict(p: &std::path::Path) {
    // Remove inherited permissions and grant only the current user.
    if let Ok(user) = std::env::var("USERNAME") {
        let _ = std::process::Command::new("icacls")
            .arg(p)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:F"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict(_p: &std::path::Path) {}

/// The key for an env-var name: the shell environment wins, then the stored file.
pub fn get(name: &str) -> Option<String> {
    if name.is_empty() || !is_env_name(name) {
        return None;
    }
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    read_all().remove(name)
}

pub fn source(name: &str) -> KeySource {
    if name.is_empty() || !is_env_name(name) {
        return KeySource::None;
    }
    if std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false) {
        KeySource::Shell
    } else if read_all().contains_key(name) {
        KeySource::Stored
    } else {
        KeySource::None
    }
}

pub fn set(name: &str, value: &str) -> Result<()> {
    if !is_env_name(name) {
        bail!("'{name}' is not a valid variable name (use letters, digits and _, e.g. HERMES_API_KEY)");
    }
    let value = value.trim();
    if value.is_empty() {
        bail!("the key is empty");
    }
    if value.contains('\n') || value.contains('\r') {
        bail!("the key must be a single line");
    }
    let mut m = read_all();
    m.insert(name.to_string(), value.to_string());
    write_all(&m)
}

pub fn remove(name: &str) -> Result<bool> {
    let mut m = read_all();
    let had = m.remove(name).is_some();
    if had {
        write_all(&m)?;
    }
    Ok(had)
}

pub fn stored_names() -> Vec<String> {
    read_all().into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_shell_precedence() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("AIWS_HOME", &dir) };
        unsafe { std::env::remove_var("AIWS_TEST_KEY_A") };
        assert_eq!(source("AIWS_TEST_KEY_A"), KeySource::None);
        set("AIWS_TEST_KEY_A", "  abc123  ").unwrap();
        assert_eq!(get("AIWS_TEST_KEY_A").as_deref(), Some("abc123"));
        assert_eq!(source("AIWS_TEST_KEY_A"), KeySource::Stored);
        unsafe { std::env::set_var("AIWS_TEST_KEY_A", "fromshell") };
        assert_eq!(get("AIWS_TEST_KEY_A").as_deref(), Some("fromshell"));
        assert_eq!(source("AIWS_TEST_KEY_A"), KeySource::Shell);
        unsafe { std::env::remove_var("AIWS_TEST_KEY_A") };
        assert!(remove("AIWS_TEST_KEY_A").unwrap());
        assert!(get("AIWS_TEST_KEY_A").is_none());
        assert!(set("bad name!", "x").is_err());
        assert!(set("OK_NAME", "line1\nline2").is_err());
    }
}
