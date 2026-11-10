//! Encodes crossterm key events as the bytes a terminal sends to a PTY child.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// `app_cursor` is the child's DECCKM mode (arrows send `ESC O x` instead of `ESC [ x`).
pub fn encode(key: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let cursor = |c: u8| {
        if app_cursor { vec![0x1b, b'O', c] } else { vec![0x1b, b'[', c] }
    };
    let mut out = match key.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            ' ' | '@' => vec![0x00],
            l @ 'a'..='z' => vec![l as u8 - b'a' + 1],
            '[' => vec![0x1b],
            '\\' => vec![0x1c],
            ']' => vec![0x1d],
            _ => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => b"\x1b[13;2u".to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor(b'A'),
        KeyCode::Down => cursor(b'B'),
        KeyCode::Right => cursor(b'C'),
        KeyCode::Left => cursor(b'D'),
        KeyCode::Home => cursor(b'H'),
        KeyCode::End => cursor(b'F'),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + (n - 1)],
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            format!("\x1b[{code}~").into_bytes()
        }
        _ => Vec::new(),
    };
    if alt && !out.is_empty() && matches!(key.code, KeyCode::Char(_)) {
        out.insert(0, 0x1b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn plain(code: KeyCode) -> Vec<u8> {
        encode(k(code, KeyModifiers::NONE), false)
    }

    #[test]
    fn printable_and_utf8_chars() {
        assert_eq!(plain(KeyCode::Char('a')), b"a");
        assert_eq!(encode(k(KeyCode::Char('A'), KeyModifiers::SHIFT), false), b"A");
        assert_eq!(plain(KeyCode::Char('ñ')), "ñ".as_bytes());
    }

    #[test]
    fn control_and_alt_chords() {
        assert_eq!(encode(k(KeyCode::Char('c'), KeyModifiers::CONTROL), false), [0x03]);
        assert_eq!(encode(k(KeyCode::Char('a'), KeyModifiers::CONTROL), false), [0x01]);
        assert_eq!(encode(k(KeyCode::Char(' '), KeyModifiers::CONTROL), false), [0x00]);
        assert_eq!(encode(k(KeyCode::Char('x'), KeyModifiers::ALT), false), b"\x1bx");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(plain(KeyCode::Enter), b"\r");
        assert_eq!(encode(k(KeyCode::Enter, KeyModifiers::SHIFT), false), b"\x1b[13;2u");
        assert_eq!(plain(KeyCode::Backspace), [0x7f]);
        assert_eq!(plain(KeyCode::Tab), b"\t");
        assert_eq!(plain(KeyCode::BackTab), b"\x1b[Z");
        assert_eq!(plain(KeyCode::Esc), b"\x1b");
        assert_eq!(plain(KeyCode::Delete), b"\x1b[3~");
        assert_eq!(plain(KeyCode::PageUp), b"\x1b[5~");
        assert_eq!(plain(KeyCode::PageDown), b"\x1b[6~");
    }

    #[test]
    fn arrows_follow_cursor_mode() {
        assert_eq!(plain(KeyCode::Up), b"\x1b[A");
        assert_eq!(plain(KeyCode::Left), b"\x1b[D");
        assert_eq!(plain(KeyCode::Home), b"\x1b[H");
        assert_eq!(encode(k(KeyCode::Down, KeyModifiers::NONE), true), b"\x1bOB");
        assert_eq!(encode(k(KeyCode::End, KeyModifiers::NONE), true), b"\x1bOF");
    }

    #[test]
    fn function_keys() {
        assert_eq!(plain(KeyCode::F(1)), b"\x1bOP");
        assert_eq!(plain(KeyCode::F(4)), b"\x1bOS");
        assert_eq!(plain(KeyCode::F(5)), b"\x1b[15~");
        assert_eq!(plain(KeyCode::F(12)), b"\x1b[24~");
    }

    #[test]
    fn unsupported_keys_send_nothing() {
        assert!(plain(KeyCode::Null).is_empty());
        assert!(plain(KeyCode::F(30)).is_empty());
    }
}
