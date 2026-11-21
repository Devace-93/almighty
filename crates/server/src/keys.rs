//! Named keys -> bytes for a PTY, using the terminal's current input modes.
//!
//! Syntax: `Enter`, `Tab`, `Esc`, `Up`, `F5`, `C-c` (Ctrl), `M-x` (Alt),
//! `S-Tab`, combinations like `C-M-a`, or a single literal character.

#[derive(Debug, Clone, Copy, Default)]
pub struct Modes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct KeyError(pub String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown key: {}", self.0)
    }
}

/// xterm modifier parameter: 1 + (shift=1 | alt=2 | ctrl=4).
fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2)
}

pub fn encode_key(spec: &str, modes: Modes) -> Result<Vec<u8>, KeyError> {
    let err = || KeyError(spec.to_string());
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut rest = spec;
    // Strip modifier prefixes, but leave a bare "-" or names like "C-" + "-" intact.
    loop {
        let mut chars = rest.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some('C'), Some('-'), Some(_)) => ctrl = true,
            (Some('M'), Some('-'), Some(_)) | (Some('A'), Some('-'), Some(_)) => alt = true,
            (Some('S'), Some('-'), Some(_)) => shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let modified = ctrl || alt || shift;
    let m = modifier_param(shift, alt, ctrl);
    let lower = rest.to_ascii_lowercase();

    // Cursor and navigation keys with a final letter (CSI/SS3 form).
    let letter = match lower.as_str() {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(l) = letter {
        return Ok(if modified {
            format!("\x1b[1;{m}{l}").into_bytes()
        } else if modes.application_cursor {
            format!("\x1bO{l}").into_bytes()
        } else {
            format!("\x1b[{l}").into_bytes()
        });
    }

    // Keys in the `CSI <n> ~` family.
    let tilde = match lower.as_str() {
        "insert" | "ins" => Some(2),
        "delete" | "del" => Some(3),
        "pageup" | "pgup" => Some(5),
        "pagedown" | "pgdn" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(n) = tilde {
        return Ok(if modified {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        });
    }

    // F1-F4 use SS3 unmodified, CSI 1;m <letter> modified.
    let f14 = match lower.as_str() {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    };
    if let Some(l) = f14 {
        return Ok(if modified {
            format!("\x1b[1;{m}{l}").into_bytes()
        } else {
            format!("\x1bO{l}").into_bytes()
        });
    }

    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match lower.as_str() {
        "enter" | "return" | "cr" => out.push(b'\r'),
        "tab" => {
            if shift {
                // Back-tab; an Alt prefix (already pushed) is kept.
                out.extend_from_slice(b"\x1b[Z");
            } else {
                out.push(b'\t');
            }
        }
        "esc" | "escape" => out.push(0x1b),
        "space" | "spc" => out.push(if ctrl { 0x00 } else { b' ' }),
        "backspace" | "bs" => out.push(if ctrl { 0x08 } else { 0x7f }),
        _ => {
            let mut chars = rest.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(err());
            };
            if ctrl {
                let b = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | '2' => 0x00,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '7' | '/' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => return Err(err()),
                };
                out.push(b);
            } else {
                let c = if shift { c.to_ascii_uppercase() } else { c };
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    Ok(out)
}

/// Text typed as a block: wrapped in bracketed-paste markers when the
/// application asked for them, so editors do not auto-indent or run it early.
pub fn encode_text(text: &str, modes: Modes) -> Vec<u8> {
    if modes.bracketed_paste && (text.contains('\n') || text.contains('\r')) {
        // A paste must not be able to close the bracket itself.
        let clean = text.replace("\x1b[201~", "");
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

/// Removes device-status queries (`ESC[5n`, `ESC[6n`) from a byte stream
/// sent to an attached terminal. The server's emulator already answers them;
/// a real terminal would answer again and the application would read the
/// reply twice. Keeps state across chunks, since a query can be split.
#[derive(Debug, Default)]
pub struct QueryFilter {
    held: Vec<u8>,
}

impl QueryFilter {
    pub fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() + self.held.len());
        for &b in chunk {
            self.held.push(b);
            match self.held.as_slice() {
                [0x1b] | [0x1b, b'['] | [0x1b, b'[', b'5' | b'6'] => {} // may still be a query
                [0x1b, b'[', b'5' | b'6', b'n'] => self.held.clear(),   // drop it
                _ => {
                    // Not a query. The last byte may start a new sequence.
                    let last = self.held.pop();
                    out.append(&mut self.held);
                    match last {
                        Some(0x1b) => self.held.push(0x1b),
                        Some(b) => out.push(b),
                        None => {}
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Vec<u8> {
        encode_key(s, Modes::default()).unwrap()
    }

    #[test]
    fn basics() {
        assert_eq!(k("Enter"), b"\r");
        assert_eq!(k("Tab"), b"\t");
        assert_eq!(k("S-Tab"), b"\x1b[Z");
        assert_eq!(k("Esc"), b"\x1b");
        assert_eq!(k("C-c"), [0x03]);
        assert_eq!(k("C-C"), [0x03]);
        assert_eq!(k("M-x"), b"\x1bx");
        assert_eq!(k("C-M-a"), [0x1b, 0x01]);
        assert_eq!(k("a"), b"a");
        assert_eq!(k("-"), b"-");
        assert_eq!(k("C"), b"C");
        assert_eq!(k("ñ"), "ñ".as_bytes());
        assert_eq!(k("Backspace"), [0x7f]);
        assert_eq!(k("C-Space"), [0x00]);
    }

    #[test]
    fn cursor_modes() {
        assert_eq!(k("Up"), b"\x1b[A");
        let app = Modes { application_cursor: true, ..Default::default() };
        assert_eq!(encode_key("Up", app).unwrap(), b"\x1bOA");
        // Modified arrows always use the CSI form.
        assert_eq!(encode_key("C-Right", app).unwrap(), b"\x1b[1;5C");
        assert_eq!(k("S-M-Left"), b"\x1b[1;4D");
    }

    #[test]
    fn function_and_nav() {
        assert_eq!(k("F1"), b"\x1bOP");
        assert_eq!(k("F5"), b"\x1b[15~");
        assert_eq!(k("C-F5"), b"\x1b[15;5~");
        assert_eq!(k("PageDown"), b"\x1b[6~");
        assert_eq!(k("Delete"), b"\x1b[3~");
        assert_eq!(k("Home"), b"\x1b[H");
    }

    #[test]
    fn errors() {
        assert!(encode_key("Bogus", Modes::default()).is_err());
        assert!(encode_key("C-é", Modes::default()).is_err());
        assert!(encode_key("", Modes::default()).is_err());
    }

    #[test]
    fn query_filter() {
        let mut f = QueryFilter::default();
        assert_eq!(f.filter(b"ab\x1b[6ncd"), b"abcd");
        assert_eq!(f.filter(b"\x1b[5n"), b"");
        // Other sequences pass untouched, including ones that share a prefix.
        assert_eq!(f.filter(b"\x1b[6;1H\x1b[5m\x1b[0n"), b"\x1b[6;1H\x1b[5m\x1b[0n");
        assert_eq!(f.filter(b"\x1b\x1b[6nx"), b"\x1bx");
        // Split across chunks.
        assert_eq!(f.filter(b"x\x1b"), b"x");
        assert_eq!(f.filter(b"[6"), b"");
        assert_eq!(f.filter(b"ny"), b"y");
        assert_eq!(f.filter(b"\x1b[6"), b"");
        assert_eq!(f.filter(b"A"), b"\x1b[6A");
    }

    #[test]
    fn paste() {
        let on = Modes { bracketed_paste: true, ..Default::default() };
        assert_eq!(encode_text("ls", on), b"ls");
        assert_eq!(encode_text("a\nb", on), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(encode_text("a\nb", Modes::default()), b"a\nb");
        assert_eq!(encode_text("a\n\x1b[201~rm", on), b"\x1b[200~a\nrm\x1b[201~");
    }
}
