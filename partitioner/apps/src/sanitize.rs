//! Engine-side display sanitization for untrusted text (T35, S31, audit F1).
//!
//! GitHub titles/bodies/comments and Slack message text, channel and user names
//! are attacker-controllable. They are sanitized before they enter the NDJSON
//! protocol so a C0/C1 control byte, ESC, or a Unicode bidi override can never
//! reach the partitioner (and, through it, the real terminal). This is the first
//! of two defensive layers; the partitioner sanitizes again at its rendering
//! boundary. Multi-line text keeps `\n` (tabs become spaces); single-line labels
//! collapse to one line.

/// True for a code point that must never reach the terminal verbatim: the C0
/// controls (`0x00`–`0x1F`, including ESC `0x1B`), DEL (`0x7F`), the C1 controls
/// (`0x80`–`0x9F`) and the Unicode bidi overrides/isolates (`U+202A`–`U+202E`,
/// `U+2066`–`U+2069`).
pub fn is_unsafe(ch: char) -> bool {
    let c = ch as u32;
    c < 0x20
        || c == 0x7f
        || (0x80..=0x9f).contains(&c)
        || ('\u{202A}'..='\u{202E}').contains(&ch)
        || ('\u{2066}'..='\u{2069}').contains(&ch)
}

/// Sanitizes multi-line untrusted text: keeps `\n`, turns `\t` into a space,
/// drops every other control/escape byte and bidi override, and preserves
/// ordinary Unicode (accents, emoji, CJK). Idempotent.
pub fn sanitize_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if is_unsafe(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Single-line variant for titles and labels: like [`sanitize_display`] but also
/// collapses newlines (and tabs) to spaces. Idempotent.
pub fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\n' | '\t' => out.push(' '),
            c if is_unsafe(c) => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_and_escape_keeps_newlines_and_unicode() {
        let evil = "a\x1b]0;pwn\x07b\x1b[2J\nc\u{202e}d café 🚀";
        let out = sanitize_display(evil);
        assert_eq!(out, "a]0;pwnb[2J\ncd café 🚀");
        assert!(!out.contains('\x1b'));
        assert!(out.contains('\n'));
        assert!(!out.chars().any(|c| is_unsafe(c) && c != '\n'));
    }

    #[test]
    fn line_collapses_newlines() {
        assert_eq!(sanitize_line("a\nb\x1bc"), "a bc");
    }

    #[test]
    fn idempotent() {
        let once = sanitize_display("\x1b]0;x\x07\t\u{202e}ok");
        assert_eq!(sanitize_display(&once), once);
    }
}
