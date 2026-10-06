//! Desktop notifications (P2.8): BEL + OSC 9/99/777, focus-gated.
//!
//! Focus tracking uses DECSET 1004 (`EnableFocusChange`) — the terminal
//! reports FocusGained/FocusLost as input events, and the app only
//! notifies while unfocused. All three OSC flavors are emitted:
//! terminals ignore the ones they don't implement (OSC 9 → iTerm2 /
//! WezTerm / Windows Terminal toast, OSC 99 → kitty, OSC 777 → urxvt).

use std::io::Write;

/// Text bound for the real terminal outside ratatui's buffer (exit
/// handoff, line mode, notifications): drops every ESC-introduced
/// sequence (CSI, OSC, DCS, APC, PM, SOS, two-byte escapes), the C1
/// controls U+0080–U+009F, and every C0 control except `\n` and `\t`.
/// Overseer's own OSC 8 / OSC 133 framing is added after this runs.
pub fn sanitize(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\x1b' => i = skip_escape(&c, i),
            ch @ ('\n' | '\t') => {
                out.push(ch);
                i += 1;
            }
            ch if ch.is_control() => i += 1,
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// Index just past the escape sequence that starts at `c[i]` (ESC).
fn skip_escape(c: &[char], i: usize) -> usize {
    match c.get(i + 1) {
        None => i + 1,
        // CSI: parameter/intermediate bytes, then a final 0x40–0x7E.
        Some('[') => {
            let mut j = i + 2;
            while let Some(&ch) = c.get(j) {
                match ch {
                    '\x40'..='\x7e' => return j + 1,
                    '\x20'..='\x3f' => j += 1,
                    _ => return j,
                }
            }
            j
        }
        // OSC/DCS/APC/PM/SOS strings end at BEL or ST (ESC \ or U+009C).
        // Unterminated: drop just the introducer — the rest is filtered
        // as plain text, so nothing executes and nothing is lost.
        Some(']' | 'P' | '_' | '^' | 'X') => {
            let mut j = i + 2;
            while let Some(&ch) = c.get(j) {
                match ch {
                    '\x07' | '\u{9c}' => return j + 1,
                    '\x1b' if c.get(j + 1) == Some(&'\\') => return j + 2,
                    '\x1b' => return j,
                    _ => j += 1,
                }
            }
            i + 2
        }
        // Other escapes: intermediates 0x20–0x2F, then a final 0x30–0x7E.
        Some(_) => {
            let mut j = i + 1;
            while matches!(c.get(j), Some('\x20'..='\x2f')) {
                j += 1;
            }
            if matches!(c.get(j), Some('\x30'..='\x7e')) {
                j + 1
            } else {
                j
            }
        }
    }
}

/// OSC payloads are one line: sanitized, then `\n`/`\t` become spaces.
fn clean(s: &str) -> String {
    sanitize(s)
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
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
    fn sanitize_drops_sequences_and_controls() {
        let s = sanitize(
            "a\x1b[1;31mb\x1b]8;;http://x\x1b\\c\x1bPq#\x1b\\d\x1b_apc\x07e\x1b(Bf\u{9b}g\r\x00h\n\ti\x1b]0;open",
        );
        assert_eq!(s, "abcdefgh\n\ti0;open");
    }

    #[test]
    fn osc_wrappers_shape() {
        assert!(osc8("file:///x", "x").contains("]8;;file:///x"));
        assert_eq!(osc52("f"), "\x1b]52;c;Zg==\x07");
        assert_eq!(osc133("A"), "\x1b]133;A\x07");
    }
}
