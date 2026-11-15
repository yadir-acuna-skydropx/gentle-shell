//! Display sanitization for untrusted text at the TUI rendering boundary
//! (T35, S31, audit finding F1).
//!
//! Untrusted text — GitHub issue/PR/Slack titles, bodies and comments, engine
//! item titles/previews/breadcrumbs, LLM streamed output (explain/helper/arch),
//! git commit subjects/refs and engine error strings — must never carry
//! terminal control or escape bytes to the real terminal, nor Unicode bidi
//! overrides that could spoof or scramble the display. A malicious issue body
//! containing `\x1b]52;...`, `\x1b]0;title\x07` or CSI sequences could otherwise
//! reach the host terminal or corrupt the TUI.
//!
//! These helpers are display-only: they never mutate stored data (the editor
//! buffer, the file on disk). They are deliberately NOT applied to the vt100 PTY
//! grids, which are terminal emulators by design.

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

/// Sanitizes multi-line untrusted text for display: keeps `\n`, turns `\t` into
/// a single space, drops every other control/escape byte and bidi override, and
/// preserves ordinary Unicode (accents, emoji, CJK). Idempotent.
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

/// Single-line variant for titles, refs, breadcrumbs and status lines: like
/// [`sanitize_display`] but also collapses newlines (and tabs) to spaces so the
/// text can never break the one-line layout. Idempotent.
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

/// Column-preserving display mapping for editor/viewer code cells (display only
/// — the stored buffer and the saved file are untouched, S1). An unsafe control
/// byte or a tab becomes a single space so a raw ESC in a viewed file cannot
/// drive the terminal, while every visible column stays aligned with the cursor
/// math (each source char still maps to exactly one display column).
pub fn display_char(ch: char) -> char {
    if ch == '\t' || is_unsafe(ch) {
        ' '
    } else {
        ch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_csi_osc_control_bytes() {
        // OSC 0 (set window title) + BEL, OSC 52 clipboard, CSI 2J clear.
        let evil = "a\x1b]0;pwn\x07b\x1b]52;c;ZXZpbA==\x07c\x1b[2Jd\x1b[31me";
        let out = sanitize_display(evil);
        assert_eq!(out, "a]0;pwnb]52;c;ZXZpbA==c[2Jd[31me");
        assert!(!out.contains('\x1b'));
        assert!(!out.chars().any(is_unsafe));
    }

    #[test]
    fn strips_c1_del_and_bidi_overrides_but_keeps_newlines() {
        let evil = "line1\nx\u{7f}y\u{85}z\u{202e}rtl\u{202c}\u{2066}iso\u{2069}";
        let out = sanitize_display(evil);
        assert_eq!(out, "line1\nxyzrtliso");
        assert!(out.contains('\n'));
        assert!(out.chars().all(|c| c == '\n' || !is_unsafe(c)));
    }

    #[test]
    fn keeps_normal_unicode_accents_emoji_cjk() {
        let s = "café — 日本語 🚀 naïve";
        assert_eq!(sanitize_display(s), s);
    }

    #[test]
    fn tab_becomes_space_and_is_idempotent() {
        let s = "a\tb\tc";
        let once = sanitize_display(s);
        assert_eq!(once, "a b c");
        assert_eq!(sanitize_display(&once), once);
    }

    #[test]
    fn idempotent_on_hostile_input() {
        let evil = "\x1b]0;x\x07\t\u{202e}\n\u{9f}ok";
        let once = sanitize_display(evil);
        assert_eq!(sanitize_display(&once), once);
    }

    #[test]
    fn sanitize_line_collapses_newlines_and_tabs_to_spaces() {
        let out = sanitize_line("a\nb\tc\x1bd");
        assert_eq!(out, "a b cd");
        assert!(!out.contains('\n'));
    }

    #[test]
    fn display_char_maps_unsafe_and_tab_to_space_keeps_normal() {
        assert_eq!(display_char('\x1b'), ' ');
        assert_eq!(display_char('\t'), ' ');
        assert_eq!(display_char('\u{202e}'), ' ');
        assert_eq!(display_char('\u{9f}'), ' ');
        assert_eq!(display_char('é'), 'é');
        assert_eq!(display_char('a'), 'a');
    }
}
