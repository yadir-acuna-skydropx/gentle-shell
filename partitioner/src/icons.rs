//! Nerd Font icons for the sidebar tree and the Files overlay (S4, T10).
//!
//! The default set uses Nerd Font glyphs (codepoints from the nerd-fonts cheat
//! sheet: nf-fa-*, nf-seti-*, nf-dev-*, nf-custom-*), each a single Private Use
//! Area codepoint that `unicode-width` (the same crate ratatui uses to measure
//! cells) reports as one column. Set `GENTLE_PARTITIONER_ICONS=ascii` to fall
//! back to plain ASCII markers for terminals without the font installed.
use ratatui::style::Color;

use crate::tree::Entry;

/// Which icon set to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconMode {
    /// Nerd Font glyphs (default).
    Nerd,
    /// Plain ASCII markers (`▸`/`▾` for folders, nothing for files).
    Ascii,
}

impl IconMode {
    /// `GENTLE_PARTITIONER_ICONS=ascii` selects the ASCII fallback; anything
    /// else (including unset) selects the Nerd Font set.
    pub fn from_env() -> Self {
        match std::env::var("GENTLE_PARTITIONER_ICONS").as_deref() {
            Ok("ascii") => IconMode::Ascii,
            _ => IconMode::Nerd,
        }
    }
}

// Folder glyphs.
const FOLDER: &str = "\u{f07b}"; // nf-fa-folder
const FOLDER_OPEN: &str = "\u{f07c}"; // nf-fa-folder_open
// File glyphs by language / kind.
const PYTHON: &str = "\u{e606}"; // nf-seti-python
const RUST: &str = "\u{e7a8}"; // nf-seti-rust
const JS: &str = "\u{e60c}"; // nf-seti-javascript
const TS: &str = "\u{e628}"; // nf-seti-typescript
const REACT: &str = "\u{e7ba}"; // nf-seti-react
const JSON: &str = "\u{e60b}"; // nf-seti-json
const MARKDOWN: &str = "\u{e609}"; // nf-seti-markdown
const CONFIG: &str = "\u{e615}"; // nf-seti-config (toml/yaml)
const SHELL: &str = "\u{f489}"; // nf-oct-terminal
const SQL: &str = "\u{e706}"; // nf-dev-database
const HTML: &str = "\u{e60e}"; // nf-seti-html
const CSS: &str = "\u{e614}"; // nf-seti-css
const GO: &str = "\u{e627}"; // nf-seti-go
const JAVA: &str = "\u{e738}"; // nf-dev-java
const CLANG: &str = "\u{e61e}"; // nf-custom-c
const CPP: &str = "\u{e61d}"; // nf-custom-cpp
const LOCK: &str = "\u{f023}"; // nf-fa-lock
const TXT: &str = "\u{f0f6}"; // nf-fa-file_text_o
const GIT: &str = "\u{e702}"; // nf-dev-git
const DOCKER: &str = "\u{e7b0}"; // nf-dev-docker
const MAKE: &str = "\u{f0ad}"; // nf-fa-wrench
const NPM: &str = "\u{e71e}"; // nf-dev-npm
const IMAGE: &str = "\u{f1c5}"; // nf-fa-file_image_o
const FILE: &str = "\u{f15b}"; // nf-fa-file (generic default)

/// Every glyph the Nerd Font set can render; used to assert each is one column.
pub const GLYPHS: &[&str] = &[
    FOLDER, FOLDER_OPEN, PYTHON, RUST, JS, TS, REACT, JSON, MARKDOWN, CONFIG, SHELL, SQL, HTML, CSS, GO, JAVA, CLANG,
    CPP, LOCK, TXT, GIT, DOCKER, MAKE, NPM, IMAGE, FILE,
];

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Returns the glyph and colour to draw before `entry`. For directories,
/// `expanded` chooses the open or closed folder glyph.
pub fn icon_for(entry: &Entry, expanded: bool, mode: IconMode) -> (&'static str, Color) {
    match mode {
        IconMode::Ascii => ascii_icon(entry, expanded),
        IconMode::Nerd => nerd_icon(entry, expanded),
    }
}

/// ASCII fallback: `▸`/`▾` for folders, nothing for files (default colour).
fn ascii_icon(entry: &Entry, expanded: bool) -> (&'static str, Color) {
    if entry.is_dir {
        (if expanded { "▾" } else { "▸" }, Color::Reset)
    } else {
        ("", Color::Reset)
    }
}

fn nerd_icon(entry: &Entry, expanded: bool) -> (&'static str, Color) {
    if entry.is_dir {
        return (if expanded { FOLDER_OPEN } else { FOLDER }, rgb(97, 175, 239));
    }
    // Well-known file names take priority over the extension.
    match entry.name.as_str() {
        "Dockerfile" | "dockerfile" | ".dockerignore" => return (DOCKER, rgb(29, 99, 237)),
        "Makefile" | "makefile" | "GNUmakefile" => return (MAKE, rgb(150, 150, 150)),
        "Cargo.toml" | "Cargo.lock" => return (RUST, rgb(222, 98, 42)),
        "package.json" | "package-lock.json" => return (NPM, rgb(203, 56, 55)),
        _ => {}
    }
    if entry.name.starts_with(".git") {
        return (GIT, rgb(240, 80, 51));
    }
    match entry.format.as_str() {
        "py" => (PYTHON, rgb(53, 114, 165)),
        "rs" => (RUST, rgb(222, 98, 42)),
        "js" | "mjs" | "cjs" => (JS, rgb(240, 219, 79)),
        "ts" => (TS, rgb(49, 120, 198)),
        "tsx" | "jsx" => (REACT, rgb(97, 218, 251)),
        "json" => (JSON, rgb(203, 196, 88)),
        "md" | "markdown" => (MARKDOWN, rgb(102, 153, 204)),
        "toml" => (CONFIG, rgb(156, 66, 33)),
        "yaml" | "yml" => (CONFIG, rgb(203, 75, 22)),
        "sh" | "bash" | "zsh" => (SHELL, rgb(137, 224, 81)),
        "sql" => (SQL, rgb(227, 124, 37)),
        "html" | "htm" => (HTML, rgb(227, 76, 38)),
        "css" | "scss" | "sass" => (CSS, rgb(21, 114, 182)),
        "go" => (GO, rgb(0, 173, 216)),
        "java" => (JAVA, rgb(176, 114, 25)),
        "c" | "h" => (CLANG, rgb(85, 137, 196)),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => (CPP, rgb(0, 89, 156)),
        "lock" => (LOCK, rgb(135, 135, 135)),
        "txt" => (TXT, rgb(180, 180, 180)),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" => (IMAGE, rgb(163, 110, 202)),
        _ => (FILE, rgb(150, 150, 150)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use unicode_width::UnicodeWidthStr;

    fn file(name: &str) -> Entry {
        let format = std::path::Path::new(name)
            .extension()
            .map_or_else(|| "-".to_string(), |e| e.to_string_lossy().to_lowercase());
        Entry { path: PathBuf::from(name), name: name.into(), depth: 0, is_dir: false, size: 0, format, status: None }
    }
    fn dir(name: &str) -> Entry {
        Entry { path: PathBuf::from(name), name: name.into(), depth: 0, is_dir: true, size: 0, format: "dir".into(), status: None }
    }

    #[test]
    fn python_and_rust_files_get_their_brand_glyph() {
        assert_eq!(icon_for(&file("app.py"), false, IconMode::Nerd), (PYTHON, rgb(53, 114, 165)));
        assert_eq!(icon_for(&file("lib.rs"), false, IconMode::Nerd), (RUST, rgb(222, 98, 42)));
    }

    #[test]
    fn folders_switch_glyph_when_open() {
        assert_eq!(icon_for(&dir("src"), false, IconMode::Nerd).0, FOLDER);
        assert_eq!(icon_for(&dir("src"), true, IconMode::Nerd).0, FOLDER_OPEN);
    }

    #[test]
    fn unknown_extension_falls_back_to_the_generic_file_glyph() {
        assert_eq!(icon_for(&file("data.xyz"), false, IconMode::Nerd).0, FILE);
        assert_eq!(icon_for(&file("noext"), false, IconMode::Nerd).0, FILE);
    }

    #[test]
    fn well_known_names_match_by_name_not_extension() {
        assert_eq!(icon_for(&file("Dockerfile"), false, IconMode::Nerd).0, DOCKER);
        assert_eq!(icon_for(&file("Makefile"), false, IconMode::Nerd).0, MAKE);
        // Cargo.toml is Rust, not the generic toml config glyph.
        assert_eq!(icon_for(&file("Cargo.toml"), false, IconMode::Nerd).0, RUST);
        assert_eq!(icon_for(&file("package.json"), false, IconMode::Nerd).0, NPM);
        assert_eq!(icon_for(&file(".gitignore"), false, IconMode::Nerd).0, GIT);
    }

    #[test]
    fn ascii_fallback_marks_folders_and_leaves_files_bare() {
        assert_eq!(icon_for(&dir("src"), false, IconMode::Ascii), ("▸", Color::Reset));
        assert_eq!(icon_for(&dir("src"), true, IconMode::Ascii), ("▾", Color::Reset));
        assert_eq!(icon_for(&file("app.py"), false, IconMode::Ascii), ("", Color::Reset));
    }

    #[test]
    fn every_nerd_glyph_is_exactly_one_column_wide() {
        for g in GLYPHS {
            assert_eq!(g.chars().count(), 1, "glyph {g:?} must be a single codepoint");
            assert_eq!(UnicodeWidthStr::width(*g), 1, "glyph {g:?} must be one column wide");
        }
    }
}
