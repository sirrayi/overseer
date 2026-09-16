//! Desktop notifications (P2.8): BEL + OSC 9/99/777, focus-gated.
//!
//! Focus tracking uses DECSET 1004 (`EnableFocusChange`) — the terminal
//! reports FocusGained/FocusLost as input events, and the app only
//! notifies while unfocused. All three OSC flavors are emitted:
//! terminals ignore the ones they don't implement (OSC 9 → iTerm2 /
//! WezTerm / Windows Terminal toast, OSC 99 → kitty, OSC 777 → urxvt).

use std::io::Write;

/// Strip bytes that could break out of an OSC string — titles/bodies
/// can carry model text, so ESC and BEL are non-negotiable.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\x1b' | '\x07' | '\u{9c}' | '\n' | '\r' => ' ',
            _ => c,
        })
        .collect()
}

/// Emit a desktop notification. Cheap unconditional — terminals that
/// don't speak a flavor ignore it.
pub fn emit(out: &mut impl Write, title: &str, body: &str) {
    let title = clean(title);
    let body = clean(body);
    let _ = write!(
        out,
        "\x07\x1b]9;{title}: {body}\x07\x1b]99;i=1:d=0;{title}\x1b]99;i=1:p=body;{body}\x07\x1b]777;notify;{title};{body}\x07"
    );
    let _ = out.flush();
}

/// OSC 8 hyperlink sequence for `text` linking to `url`.
pub fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", clean(url), clean(text))
}

/// OSC 52 clipboard write (base64 payload, `c` selection).
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", b64(text.as_bytes()))
}

/// OSC 133 shell-integration mark (`A` prompt, `B` pre-output,
/// `C` output start, `D;code` output end).
pub fn osc133(mark: &str) -> String {
    format!("\x1b]133;{mark}\x07")
}

/// Minimal base64 (no dep): 3-byte groups → 4 chars, `=` padding.
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn clean_strips_escapes() {
        let s = clean("a\x1b]52;c;evil\x07b\nc");
        assert!(!s.contains('\x1b'));
        assert!(!s.contains('\x07'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn osc_wrappers_shape() {
        assert!(osc8("file:///x", "x").contains("]8;;file:///x"));
        assert_eq!(osc52("f"), "\x1b]52;c;Zg==\x07");
        assert_eq!(osc133("A"), "\x1b]133;A\x07");
    }
}
