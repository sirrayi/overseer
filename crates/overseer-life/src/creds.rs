//! Read-only credential helpers (port of the shared bits of Synara's
//! `providerUsage/credentials.ts` + `localCredential.ts`): capped file
//! reads, permissive JSON/TOML-lite parsing, JWT `exp` decode, the
//! sha256-base64url fingerprint Synara puts in its envelope, and the
//! `/usr/bin/security` keychain reader behind the injectable runner.
//!
//! R2: everything here is read-only — capped `read()` calls, never a
//! write, rename or chmod. R4: secret values leave this module only as
//! `AuthMaterial`, never serialized.

use std::path::Path;

use sha2::{Digest, Sha256};

/// Byte cap for credential/archive files (Synara uses ~1–10MB caps).
pub const MAX_CRED_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Cap for walking JSONL archives per file.
pub const MAX_ARCHIVE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Decoded credential material. Not Serialize — secrets never go into
/// output.
#[derive(Debug)]
pub enum AuthMaterial {
    Bearer { token: String, exp_ms: Option<i64> },
    ApiKey { key: String },
}

impl AuthMaterial {
    /// 18-char sha256-base64url fingerprint, the same scheme Synara's
    /// credentials.ts uses to label credentials in output.
    pub fn fingerprint(&self) -> String {
        fingerprint_of(match self {
            AuthMaterial::Bearer { token, .. } => token,
            AuthMaterial::ApiKey { key } => key,
        })
    }
}

/// sha256 → base64url → take 18 chars (Synara's fingerprintSecret).
pub fn fingerprint_of(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let b64 = base64url_encode(&digest);
    b64.chars().take(18).collect()
}

/// Dependency-free base64url (no padding).
pub fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Read a file strictly read-only with a size cap. Returns None for
/// missing/unreadable/oversized files — callers can't distinguish, which
/// is intentional (don't leak filesystem facts into error strings).
pub fn read_capped(path: &Path, max: usize) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() as usize > max {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Read a JSON file, None on any failure.
pub fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let text = read_capped(path, MAX_CRED_FILE_BYTES)?;
    serde_json::from_str(&text).ok()
}

/// `*.bak` — used by Codex to detect a 2025-11 rotation fallback.
pub fn bak_exists(path: &Path) -> bool {
    path.with_extension("bak").is_file()
}

/// Minimal TOML reader: flat `[section]` + `key = "value"` tables, enough
/// for `credentials.toml` files (Devin, Pi). Full TOML is out of scope
/// for a zero-dep crate.
pub fn read_toml_lite(path: &Path) -> Option<toml_lite::Table> {
    let text = read_capped(path, MAX_CRED_FILE_BYTES)?;
    Some(toml_lite::parse(&text))
}

pub mod toml_lite {
    use std::collections::BTreeMap;

    /// section → key → raw string value (quotes stripped).
    pub type Table = BTreeMap<String, BTreeMap<String, String>>;

    pub fn parse(text: &str) -> Table {
        let mut out: Table = BTreeMap::new();
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = inner.trim().to_string();
                out.entry(section.clone()).or_default();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().trim_matches('"').to_string();
            let mut value = value.trim().to_string();
            // Strip trailing comment only for bare values (a '#' inside a
            // quoted string is data).
            if !(value.starts_with('"') || value.starts_with('\'')) {
                if let Some(pos) = value.find('#') {
                    value = value[..pos].trim().to_string();
                }
            }
            if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
                || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
            {
                value = value[1..value.len() - 1].to_string();
            }
            out.entry(section.clone()).or_default().insert(key, value);
        }
        out
    }

    /// Look up `key` across the root table and every section.
    pub fn find<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
        for kv in table.values() {
            if let Some(v) = kv.get(key) {
                return Some(v.as_str());
            }
        }
        None
    }
}

/// Split a JWT and decode the payload's `exp` claim (seconds → ms).
/// Handles `xxx.yyy.zzz` and headerless `yyy.zzz` shapes — Synara's
/// jwtExpiresAt accepts both.
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let _header_or_payload = parts.next()?;
    let payload = if parts.clone().count() >= 1 {
        // three or more segments → second is payload; two → first was payload
        if parts.clone().count() >= 2 {
            parts.next()
        } else {
            Some(_header_or_payload)
        }
    } else {
        None
    }?;
    let bytes = base64url_decode(payload)?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = json.get("exp")?.as_f64()? * 1000.0;
    Some(exp as i64)
}

pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'-' | b'+' => Some(62),
            b'_' | b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for b in s.bytes() {
        if b == b'=' {
            continue;
        }
        acc = (acc << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// macOS keychain access through `/usr/bin/security` and the injectable
/// runner. Attribute-only calls (existence) are always allowed; secret
/// reads (`-w`) require `ctx.allow_keychain_secrets`.
pub mod keychain {
    use crate::connector::Ctx;

    /// `security find-generic-password -s <svc> -a <acct>` without `-w`:
    /// prints attributes only. Returns true when the item exists. Never
    /// leaks a secret.
    pub fn item_exists(ctx: &Ctx, service: &str, account: Option<&str>) -> bool {
        if !ctx.platform.is_darwin() {
            return false;
        }
        let mut args: Vec<&str> = vec!["find-generic-password", "-s", service];
        if let Some(acct) = account {
            args.push("-a");
            args.push(acct);
        }
        match ctx.runner.run("/usr/bin/security", &args, 5_000) {
            Ok(out) => out.status == 0,
            Err(_) => false,
        }
    }

    /// `security find-generic-password -s <svc> [-a <acct>] -w`: prints
    /// the secret. Requires the caller's explicit opt-in
    /// (`ctx.allow_keychain_secrets`) — the Phase 0 probe keeps that off,
    /// so this always returns None there.
    pub fn read_secret(ctx: &Ctx, service: &str, account: Option<&str>) -> Option<String> {
        if !ctx.platform.is_darwin() || !ctx.allow_keychain_secrets {
            return None;
        }
        let mut args: Vec<&str> = vec!["find-generic-password", "-s", service];
        if let Some(acct) = account {
            args.push("-a");
            args.push(acct);
        }
        args.push("-w");
        let out = ctx.runner.run("/usr/bin/security", &args, 10_000).ok()?;
        if out.status != 0 {
            return None;
        }
        let text = String::from_utf8(out.stdout).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_string())
    }

    /// Claude's keychain item is a JSON blob holding the OAuth payload —
    /// Synara `claude/accessToken.ts` CLAUDE_CODE_SERVICE.
    pub fn read_json_secret(
        ctx: &Ctx,
        service: &str,
        account: Option<&str>,
    ) -> Option<serde_json::Value> {
        let text = read_secret(ctx, service, account)?;
        super::decode_keychain_json(&text)
    }
}

/// Some CLIs store the JSON credential in the keychain hex-encoded
/// (Claude Code on macOS), others store raw JSON. Port of Synara
/// `credentials.ts` `decodeKeychainJson`: direct JSON first, then
/// hex-decode then parse.
pub fn decode_keychain_json(value: &str) -> Option<serde_json::Value> {
    let trimmed = value.trim();
    if let Ok(direct) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(direct);
    }
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if hex.len().is_multiple_of(2) && !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let bytes: Option<Vec<u8>> = hex
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
            })
            .collect();
        if let Some(bytes) = bytes {
            if let Ok(text) = String::from_utf8(bytes) {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

/// Credential expiry decision shared by all OAuth-backed connectors:
/// unknown exp → try the API (server will 401 if stale); known-expired →
/// stop as `expired` without any request (R1).
#[derive(Debug, PartialEq, Eq)]
pub enum Expiry {
    Usable,
    Expired,
    Unknown,
}

pub fn expiry(exp_ms: Option<i64>, now_ms: i64) -> Expiry {
    match exp_ms {
        Some(ms) if ms <= now_ms => Expiry::Expired,
        Some(_) => Expiry::Usable,
        None => Expiry::Unknown,
    }
}

/// A discovered credential source for `discover()` output.
pub fn file_discovery(path: &Path, contains: &'static str) -> crate::connector::Discovered {
    crate::connector::Discovered {
        kind: "file",
        location: path.display().to_string(),
        contains,
        present: path.is_file(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with_exp(exp_s: i64) -> String {
        let header = base64url_encode(b"{\"alg\":\"HS256\"}");
        let payload = base64url_encode(format!("{{\"exp\":{exp_s}}}").as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn jwt_exp_decodes() {
        let tok = jwt_with_exp(1_800_000_000);
        assert_eq!(jwt_exp_ms(&tok), Some(1_800_000_000_000));
    }

    #[test]
    fn jwt_without_exp() {
        let tok = jwt_with_exp(1_800_000_000);
        let bad = tok.split('.').next().unwrap().to_string(); // header only
        assert_eq!(jwt_exp_ms(&bad), None);
        assert_eq!(jwt_exp_ms("not-a-jwt"), None);
    }

    #[test]
    fn fingerprint_is_18_base64url() {
        let fp = fingerprint_of("s3cret-token-value");
        assert_eq!(fp.len(), 18);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn toml_lite_parses() {
        let table = toml_lite::parse(
            "default_organization = \"devin\"\n\n[default]\nApiKey = \"cog_key123\"\nother = plain # comment\n",
        );
        assert_eq!(toml_lite::find(&table, "ApiKey"), Some("cog_key123"));
        assert_eq!(toml_lite::find(&table, "other"), Some("plain"));
    }

    #[test]
    fn expiry_states() {
        assert_eq!(expiry(Some(100), 50), Expiry::Usable);
        assert_eq!(expiry(Some(100), 100), Expiry::Expired);
        assert_eq!(expiry(None, 999), Expiry::Unknown);
    }
}
