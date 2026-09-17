//! Sentinel tokens for channel secrets (P7-4).
//!
//! Frozen contract, shared with P6's credential broker (R1-F6):
//! `ovsent_` + exactly 32 lowercase hex characters. P6's `Broker`
//! supersedes this module at merge — the *format* is frozen so the two can
//! never diverge, and until then this local implementation keeps bot
//! tokens out of logs, digests, and error strings.
//!
//! Bot tokens are read from the environment only ([`load_token`]): the
//! daemon never persists a token, and anything that echoes one goes
//! through [`redact`] first.

use std::sync::atomic::{AtomicU64, Ordering};

pub const SENTINEL_PREFIX: &str = "ovsent_";
/// Lowercase hex characters after the prefix.
pub const SENTINEL_HEX: usize = 32;

/// True for a well-formed sentinel: prefix + 32 lowercase hex characters.
/// Uppercase, short, long, and non-hex bodies all fail (the format is the
/// contract — a near-miss must not pass).
pub fn is_sentinel(s: &str) -> bool {
    let Some(body) = s.strip_prefix(SENTINEL_PREFIX) else {
        return false;
    };
    body.len() == SENTINEL_HEX
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A fresh sentinel token.
pub fn new_sentinel() -> String {
    let mut out = String::with_capacity(SENTINEL_PREFIX.len() + SENTINEL_HEX);
    out.push_str(SENTINEL_PREFIX);
    for b in entropy16() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 16 bytes of entropy. Primary source is `/dev/urandom`; the fallback is
/// a clock+PID+counter mix — adequate for opaque placeholders (these are
/// not keys, they stand in for keys), never a panic path.
fn entropy16() -> [u8; 16] {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos
        ^ (u64::from(std::process::id()) << 32)
        ^ SEQ
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (&buf as *const _ as usize as u64);
    for b in buf.iter_mut() {
        // xorshift64*: one byte per step, decorrelated across the seed.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *b = (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 24) as u8;
    }
    buf
}

/// Replace every occurrence of each secret with a sentinel. 1:1 within the
/// text — one sentinel per distinct secret, so two secrets stay tellable
/// apart and a log reader can still correlate occurrences. Secrets shorter
/// than 8 characters are skipped (they would match half the alphabet).
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.len() < 8 || !out.contains(s.as_str()) {
            continue;
        }
        let sentinel = new_sentinel();
        out = out.replace(s.as_str(), &sentinel);
    }
    out
}

/// Read a token from the environment. Empty/whitespace counts as unset —
/// a config that names an empty variable must not look configured.
pub fn load_token(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_format_is_frozen() {
        // The P6/P7 shared contract: `ovsent_` + exactly 32 lowercase hex.
        for _ in 0..64 {
            let s = new_sentinel();
            assert!(
                is_sentinel(&s),
                "generated sentinel must satisfy the contract: {s}"
            );
            assert_eq!(s.len(), SENTINEL_PREFIX.len() + SENTINEL_HEX);
            assert!(s
                .chars()
                .skip(SENTINEL_PREFIX.len())
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        }
        // Near-misses fail closed.
        assert!(!is_sentinel("ovsent_"));
        assert!(!is_sentinel("ovsent_0123456789abcdef0123456789abcde")); // 31
        assert!(!is_sentinel("ovsent_0123456789abcdef0123456789abcdef0")); // 33
        assert!(!is_sentinel("ovsent_0123456789ABCDEF0123456789abcdef")); // upper
        assert!(!is_sentinel("ovsent_0123456789abcdef0123456789abcdeg")); // non-hex
        assert!(!is_sentinel("sent_0123456789abcdef0123456789abcdef"));
        // Distinct tokens: no accidental constant.
        let a = new_sentinel();
        let b = new_sentinel();
        assert_ne!(a, b);
    }

    #[test]
    fn redact_replaces_every_occurrence_1_to_1() {
        let secrets = vec!["tok-abcdefgh".to_string(), "other-secret-9".to_string()];
        let text = "tok-abcdefgh tok-abcdefgh other-secret-9 tok-abcdefgh";
        let out = redact(text, &secrets);
        assert!(!out.contains("tok-abcdefgh"));
        assert!(!out.contains("other-secret-9"));
        // One sentinel per secret: 4 occurrences of the first, 1 of the
        // second, but only two distinct sentinels.
        let sentinels: Vec<&str> = out.split_whitespace().filter(|w| is_sentinel(w)).collect();
        assert_eq!(sentinels.len(), 4);
        assert_eq!(sentinels[0], sentinels[1]);
        assert_eq!(sentinels[0], sentinels[3]);
        assert_ne!(sentinels[0], sentinels[2]);
        // Short strings are left alone (they would match everywhere).
        let short = redact("abc abc", &["abc".to_string()]);
        assert_eq!(short, "abc abc");
    }

    #[test]
    fn load_token_treats_blank_as_unset() {
        // A name that cannot be set in this environment — the point is the
        // unset path, not the value.
        assert!(load_token("OVERSEER_DEFINITELY_NOT_SET_9f3a").is_none());
    }
}
