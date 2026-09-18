//! P7-1 computer-use observation helper: takeover suppression.
//!
//! When a capture targets a credential field (`cred_field`) or the session
//! runs in `watch_mode`, pixel capture is suppressed and the tool returns a
//! metadata-only observation (dimensions + reason, never pixel bytes).
//! Pure helpers so the gate, the tool, and tests share one predicate.

use crate::agent::ComputerConfig;

/// True when pixel capture must be suppressed (metadata-only obs).
pub fn is_suppressed(cfg: &ComputerConfig, cred_field: bool, _action: &str) -> bool {
    if cfg.watch_mode {
        return true;
    }
    cred_field && cfg.takeover_pause
}

/// Metadata-only observation: dimensions + suppression reason, no pixels.
pub fn metadata_obs(px_w: u32, px_h: u32, sent_w: u32, sent_h: u32, reason: &str) -> String {
    format!(
        "capture suppressed ({reason}) — metadata only: native {px_w}x{px_h}, sent {sent_w}x{sent_h}, no pixel bytes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_predicate() {
        let cfg = ComputerConfig::default();
        assert!(is_suppressed(&cfg, true, "screenshot"));
        assert!(!is_suppressed(&cfg, false, "screenshot"));
        let off = ComputerConfig {
            takeover_pause: false,
            watch_mode: false,
            egress_deny: false,
        };
        assert!(!is_suppressed(&off, true, "screenshot"));
    }

    #[test]
    fn metadata_carries_no_pixels() {
        let obs = metadata_obs(2560, 1600, 1280, 800, "cred-field focus");
        assert!(obs.contains("2560x1600"));
        assert!(obs.contains("1280x800"));
        assert!(!obs.contains("base64"));
    }
}
