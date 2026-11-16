//! Engine-side display sanitization for untrusted scanner output (T35 rules,
//! S31/S35). Scanner findings quote attacker-influenced source lines, so before
//! any text enters a report it is stripped of C0/C1 controls, ESC, DEL and the
//! Unicode bidi overrides/isolates. This mirrors `gentle-apps`'s `sanitize`
//! module; it is duplicated here (not shared) to keep the engines crate free of
//! an apps dependency.

/// True for a code point that must never reach a terminal verbatim: the C0
/// controls (`0x00`–`0x1F`, incl. ESC), DEL (`0x7F`), the C1 controls
/// (`0x80`–`0x9F`) and the Unicode bidi overrides/isolates.
pub fn is_unsafe(ch: char) -> bool {
    let c = ch as u32;
    c < 0x20
        || c == 0x7f
        || (0x80..=0x9f).contains(&c)
        || ('\u{202A}'..='\u{202E}').contains(&ch)
        || ('\u{2066}'..='\u{2069}').contains(&ch)
}

/// Multi-line sanitize: keeps `\n`, turns `\t` into a space, drops every other
/// control/escape byte and bidi override, preserves ordinary Unicode. Idempotent.
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

/// Single-line variant for titles, rule ids and paths: also collapses newlines
/// and tabs to spaces. Idempotent.
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
    fn strips_control_and_escape_keeps_unicode() {
        let evil = "a\x1b]0;pwn\x07b\x1b[2J\nc\u{202e}d café 🚀";
        let out = sanitize_display(evil);
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\u{202e}'));
        assert!(out.contains("café 🚀"));
        assert!(out.contains('\n'));
    }

    #[test]
    fn line_collapses_newlines() {
        assert_eq!(sanitize_line("a\nb\tc"), "a b c");
    }
}
