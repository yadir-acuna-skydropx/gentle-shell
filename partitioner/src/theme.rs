//! Centralized color palette for the partitioner UI (S28).
//!
//! The user reported that dark blue (`Color::Blue`) is hard to read on the dark
//! terminal background ("el azul oscuro no es muy legible"). These named
//! constants keep foreground colors legible and consistent: dark blue is
//! replaced by a lighter blue, and `DarkGray` is reserved for dim hints only,
//! never body text that must be read. The ASCII/no-color paths keep using
//! `Color::Reset` and are unaffected.
use ratatui::style::Color;

/// Primary accent: focus borders, prompts, selected rows, hunk headers.
pub const ACCENT: Color = Color::Cyan;

/// A readable blue that replaces the hard-to-read dark `Color::Blue` (S28).
/// Light enough to stand out on a dark background.
pub const BLUE: Color = Color::Rgb(110, 170, 255);

/// Function names / identifiers in the syntax theme (was dark `Color::Blue`).
pub const FUNCTION: Color = BLUE;

/// Dim text for hints and secondary labels ONLY — never body text to read.
pub const HINT: Color = Color::DarkGray;

/// Success / additions.
pub const SUCCESS: Color = Color::Green;

/// Warning / pending state.
pub const WARN: Color = Color::Yellow;

/// Error / removals.
pub const ERROR: Color = Color::Red;

/// Keyword / mascot accent.
pub const KEYWORD: Color = Color::Magenta;

/// Rainbow bracket cycle by nesting depth (all legible on dark).
pub const BRACKETS: [Color; 4] = [Color::Yellow, Color::Magenta, Color::Cyan, Color::Green];

/// Graph lane colors so parallel branches stay distinguishable. The former
/// dark `Color::Blue` lane is replaced by the lighter [`BLUE`] (S28).
pub const GRAPH_LANES: [Color; 6] =
    [Color::Cyan, Color::Green, Color::Yellow, BLUE, Color::Magenta, Color::Red];

#[cfg(test)]
mod tests {
    use super::*;

    /// Dark `Color::Blue` must not reappear as a foreground anywhere in
    /// `partitioner/src` outside this palette module (S28). The palette owns the
    /// single readable blue so the audit stays centralized.
    #[test]
    fn no_dark_blue_outside_theme() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("theme.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            // Ignore comment lines: only real foreground usage counts, not doc
            // mentions of the former `Color::Blue`.
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("Color::Blue") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "dark Color::Blue found outside theme.rs (use theme::BLUE): {offenders:?}"
        );
    }

    #[test]
    fn blue_is_a_light_rgb() {
        assert_eq!(BLUE, Color::Rgb(110, 170, 255));
        assert_eq!(FUNCTION, BLUE);
    }
}
