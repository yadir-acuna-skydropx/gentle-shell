//! Per-language lexical syntax highlighting for the editor pane (S5, T11).
//!
//! Approach: a small hand-written tokenizer with per-language keyword, comment
//! and string rules. We deliberately avoid `syntect` (pulls the `onig` C regex
//! engine or a large fancy-regex + default syntaxes payload, growing the binary
//! and startup cost) and `tree-sitter-highlight` (a grammar crate and C build
//! per language). Our lexer is dependency-free, keeps the release binary small,
//! starts instantly and is fast enough to highlight far more than a screenful
//! per frame. Highlighting is computed only for the lines the caller asks for,
//! with a per-line cache carrying the end-of-line state so multi-line comments
//! and strings resolve correctly; the cache is invalidated from the edited line
//! down (see [`Highlighter`]).
//!
//! Limitations (documented, S5/S11): a normal single/double-quoted string that
//! is left open at end of line is treated as ending there (only Python triple
//! quotes and JS/TS template backticks span lines); Rust lifetimes like `'a`
//! may be shown as a string; [`match_bracket`] matches by nesting only and does
//! not skip brackets inside strings or comments.
use ratatui::style::{Color, Style};

/// Languages we classify; chosen from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    Rust,
    JsTs,
    Json,
    Toml,
    Yaml,
    Shell,
    Sql,
    Markdown,
    C,
    Cpp,
    Go,
    Java,
    Html,
    Css,
    Plain,
}

/// Token classes produced by the lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Text,
    Keyword,
    Str,
    Number,
    Comment,
    Function,
    Type,
    Punct,
    /// An open/close bracket at the given nesting depth (for rainbow colouring).
    Bracket(u16),
    /// A closing bracket with no matching opener.
    Unmatched,
}

/// A multi-line string that continues onto the next line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MlString {
    TripleDouble,
    TripleSingle,
    Backtick,
}

impl MlString {
    fn delim(self) -> &'static str {
        match self {
            MlString::TripleDouble => "\"\"\"",
            MlString::TripleSingle => "'''",
            MlString::Backtick => "`",
        }
    }
}

/// Lexer state carried from the end of one line to the start of the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub block_comment: bool,
    pub ml_string: Option<MlString>,
    pub depth: u16,
}

/// A contiguous run of characters sharing one class. Concatenating the `text`
/// of every segment of a line reproduces the line exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub class: Class,
}

/// Maps a lowercase file extension to a language.
pub fn lang_for_ext(ext: &str) -> Lang {
    match ext {
        "py" | "pyw" => Lang::Python,
        "rs" => Lang::Rust,
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => Lang::JsTs,
        "json" => Lang::Json,
        "toml" => Lang::Toml,
        "yaml" | "yml" => Lang::Yaml,
        "sh" | "bash" | "zsh" => Lang::Shell,
        "sql" => Lang::Sql,
        "md" | "markdown" => Lang::Markdown,
        "c" | "h" => Lang::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Lang::Cpp,
        "go" => Lang::Go,
        "java" => Lang::Java,
        "html" | "htm" => Lang::Html,
        "css" | "scss" | "sass" => Lang::Css,
        _ => Lang::Plain,
    }
}

impl Lang {
    fn line_comments(self) -> &'static [&'static str] {
        match self {
            Lang::Python | Lang::Shell | Lang::Yaml | Lang::Toml => &["#"],
            Lang::Rust | Lang::JsTs | Lang::C | Lang::Cpp | Lang::Go | Lang::Java => &["//"],
            Lang::Sql => &["--"],
            _ => &[],
        }
    }
    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Lang::Rust | Lang::JsTs | Lang::C | Lang::Cpp | Lang::Go | Lang::Java | Lang::Css | Lang::Sql => {
                Some(("/*", "*/"))
            }
            Lang::Html => Some(("<!--", "-->")),
            _ => None,
        }
    }
    fn triple_strings(self) -> bool {
        self == Lang::Python
    }
    fn backtick(self) -> bool {
        self == Lang::JsTs
    }
    fn single_quote_string(self) -> bool {
        !matches!(self, Lang::Json | Lang::Markdown | Lang::Plain)
    }
    fn case_insensitive(self) -> bool {
        self == Lang::Sql
    }
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Python => &[
                "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from", "as", "with", "try",
                "except", "finally", "raise", "pass", "break", "continue", "lambda", "yield", "global", "nonlocal",
                "in", "is", "not", "and", "or", "None", "True", "False", "async", "await", "del", "assert",
            ],
            Lang::Rust => &[
                "fn", "let", "mut", "const", "static", "struct", "enum", "trait", "impl", "pub", "use", "mod", "match",
                "if", "else", "for", "while", "loop", "return", "break", "continue", "ref", "move", "as", "where",
                "type", "dyn", "self", "Self", "super", "crate", "async", "await", "unsafe", "extern", "in", "true",
                "false",
            ],
            Lang::JsTs => &[
                "function", "return", "if", "else", "for", "while", "do", "switch", "case", "break", "continue",
                "const", "let", "var", "new", "class", "extends", "import", "from", "export", "default", "async",
                "await", "try", "catch", "finally", "throw", "typeof", "instanceof", "in", "of", "this", "super",
                "null", "undefined", "true", "false", "void", "yield", "interface", "type", "enum", "implements",
                "public", "private", "protected", "readonly", "namespace",
            ],
            Lang::Json => &["true", "false", "null"],
            Lang::Toml => &["true", "false"],
            Lang::Yaml => &["true", "false", "null", "yes", "no"],
            Lang::Shell => &[
                "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "function", "in",
                "return", "export", "local", "echo", "read", "exit", "source",
            ],
            Lang::Sql => &[
                "select", "from", "where", "insert", "update", "delete", "into", "values", "set", "create", "table",
                "drop", "alter", "join", "inner", "left", "right", "outer", "on", "group", "by", "order", "having",
                "limit", "and", "or", "not", "null", "as", "distinct", "union", "primary", "key", "foreign",
            ],
            Lang::C => &[
                "int", "char", "float", "double", "void", "long", "short", "unsigned", "signed", "struct", "union",
                "enum", "typedef", "static", "const", "extern", "return", "if", "else", "for", "while", "do", "switch",
                "case", "break", "continue", "sizeof", "goto", "default", "volatile", "register",
            ],
            Lang::Cpp => &[
                "int", "char", "float", "double", "void", "long", "short", "unsigned", "signed", "struct", "union",
                "enum", "typedef", "static", "const", "extern", "return", "if", "else", "for", "while", "do", "switch",
                "case", "break", "continue", "sizeof", "goto", "default", "class", "public", "private", "protected",
                "virtual", "template", "typename", "namespace", "using", "new", "delete", "this", "nullptr", "true",
                "false", "bool", "try", "catch", "throw", "operator", "friend", "inline", "explicit", "override",
            ],
            Lang::Go => &[
                "func", "package", "import", "var", "const", "type", "struct", "interface", "map", "chan", "go",
                "defer", "return", "if", "else", "for", "range", "switch", "case", "default", "break", "continue",
                "select", "nil", "true", "false",
            ],
            Lang::Java => &[
                "public", "private", "protected", "class", "interface", "extends", "implements", "static", "final",
                "void", "int", "long", "double", "float", "boolean", "char", "new", "return", "if", "else", "for",
                "while", "do", "switch", "case", "break", "continue", "try", "catch", "finally", "throw", "throws",
                "import", "package", "this", "super", "null", "true", "false", "abstract", "enum",
            ],
            Lang::Markdown | Lang::Html | Lang::Css | Lang::Plain => &[],
        }
    }
}

fn is_open(c: char) -> bool {
    matches!(c, '(' | '[' | '{')
}
fn is_close(c: char) -> bool {
    matches!(c, ')' | ']' | '}')
}
fn closer_for(c: char) -> char {
    match c {
        '(' => ')',
        '[' => ']',
        _ => '}',
    }
}
fn opener_for(c: char) -> char {
    match c {
        ')' => '(',
        ']' => '[',
        _ => '{',
    }
}

fn matches_at(chars: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    if i + p.len() > chars.len() {
        return false;
    }
    chars[i..i + p.len()] == p[..]
}

fn find_from(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let plen = pat.chars().count();
    if plen == 0 {
        return Some(from);
    }
    (from..=chars.len().saturating_sub(plen)).find(|&i| matches_at(chars, i, pat))
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_ident_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Lexes one line starting from `state`; returns the segments and the state to
/// feed to the next line.
pub fn highlight_line(line: &str, lang: Lang, mut state: State) -> (Vec<Seg>, State) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut segs: Vec<Seg> = Vec::new();
    let mut buf = String::new();
    let mut buf_class = Class::Text;
    let mut i = 0usize;

    // Flush the pending plain/punct run.
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                segs.push(Seg { text: std::mem::take(&mut buf), class: buf_class });
            }
        };
    }
    // Push a finished multi-character token.
    macro_rules! push_tok {
        ($text:expr, $class:expr) => {{
            flush!();
            segs.push(Seg { text: $text, class: $class });
        }};
    }

    // Continue a block comment carried from the previous line.
    if state.block_comment {
        if let Some((_, end)) = lang.block_comment() {
            if let Some(pos) = find_from(&chars, i, end) {
                let upto = pos + end.chars().count();
                push_tok!(chars[i..upto].iter().collect(), Class::Comment);
                i = upto;
                state.block_comment = false;
            } else {
                push_tok!(chars[i..].iter().collect(), Class::Comment);
                return (segs, state);
            }
        } else {
            state.block_comment = false;
        }
    }
    // Continue a multi-line string carried from the previous line.
    if let Some(ml) = state.ml_string {
        let delim = ml.delim();
        if let Some(pos) = find_from(&chars, i, delim) {
            let upto = pos + delim.chars().count();
            push_tok!(chars[i..upto].iter().collect(), Class::Str);
            i = upto;
            state.ml_string = None;
        } else {
            push_tok!(chars[i..].iter().collect(), Class::Str);
            return (segs, state);
        }
    }

    'scan: while i < n {
        let c = chars[i];

        // Line comment: the rest of the line.
        for lc in lang.line_comments() {
            if matches_at(&chars, i, lc) {
                push_tok!(chars[i..].iter().collect(), Class::Comment);
                flush!();
                return (segs, state);
            }
        }
        // Block comment start.
        if let Some((open, close)) = lang.block_comment() {
            if matches_at(&chars, i, open) {
                let after = i + open.chars().count();
                if let Some(pos) = find_from(&chars, after, close) {
                    let upto = pos + close.chars().count();
                    push_tok!(chars[i..upto].iter().collect(), Class::Comment);
                    i = upto;
                } else {
                    push_tok!(chars[i..].iter().collect(), Class::Comment);
                    state.block_comment = true;
                    return (segs, state);
                }
                continue 'scan;
            }
        }
        // Python triple-quoted strings (may span lines).
        if lang.triple_strings() {
            for ml in [MlString::TripleDouble, MlString::TripleSingle] {
                let delim = ml.delim();
                if matches_at(&chars, i, delim) {
                    let after = i + 3;
                    if let Some(pos) = find_from(&chars, after, delim) {
                        let upto = pos + 3;
                        push_tok!(chars[i..upto].iter().collect(), Class::Str);
                        i = upto;
                    } else {
                        push_tok!(chars[i..].iter().collect(), Class::Str);
                        state.ml_string = Some(ml);
                        return (segs, state);
                    }
                    continue 'scan;
                }
            }
        }
        // JS/TS template string (backtick, may span lines).
        if lang.backtick() && c == '`' {
            if let Some(pos) = find_from(&chars, i + 1, "`") {
                push_tok!(chars[i..=pos].iter().collect(), Class::Str);
                i = pos + 1;
            } else {
                push_tok!(chars[i..].iter().collect(), Class::Str);
                state.ml_string = Some(MlString::Backtick);
                return (segs, state);
            }
            continue 'scan;
        }
        // Normal single-line string.
        if c == '"' || (c == '\'' && lang.single_quote_string()) {
            let (text, next) = read_string(&chars, i, c);
            push_tok!(text, Class::Str);
            i = next;
            continue 'scan;
        }
        // Number.
        if c.is_ascii_digit() {
            let start = i;
            while i < n && (is_ident_part(chars[i]) || chars[i] == '.') {
                i += 1;
            }
            push_tok!(chars[start..i].iter().collect(), Class::Number);
            continue 'scan;
        }
        // Identifier / keyword / type / function.
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_part(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let class = classify_word(&word, lang, &chars, i);
            push_tok!(word, class);
            continue 'scan;
        }
        // Brackets with rainbow depth.
        if is_open(c) {
            flush!();
            segs.push(Seg { text: c.to_string(), class: Class::Bracket(state.depth) });
            state.depth = state.depth.saturating_add(1);
            i += 1;
            continue 'scan;
        }
        if is_close(c) {
            flush!();
            let class = if state.depth == 0 {
                Class::Unmatched
            } else {
                state.depth -= 1;
                Class::Bracket(state.depth)
            };
            segs.push(Seg { text: c.to_string(), class });
            i += 1;
            continue 'scan;
        }
        // Whitespace stays Text; other symbols are Punct.
        let class = if c.is_whitespace() { Class::Text } else { Class::Punct };
        if class != buf_class {
            flush!();
            buf_class = class;
        }
        buf.push(c);
        i += 1;
    }
    flush!();
    (segs, state)
}

fn classify_word(word: &str, lang: Lang, chars: &[char], after: usize) -> Class {
    let is_keyword = if lang.case_insensitive() {
        let lower = word.to_lowercase();
        lang.keywords().iter().any(|k| *k == lower)
    } else {
        lang.keywords().contains(&word)
    };
    if is_keyword {
        return Class::Keyword;
    }
    // A capitalised identifier reads as a type (cheap heuristic).
    if word.chars().next().is_some_and(|c| c.is_uppercase()) {
        return Class::Type;
    }
    // An identifier immediately followed by `(` reads as a function call.
    let mut j = after;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    if chars.get(j) == Some(&'(') {
        return Class::Function;
    }
    Class::Text
}

/// Reads a single-line string starting at the opening quote `quote`.
fn read_string(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut s = String::new();
    s.push(chars[start]);
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        s.push(c);
        i += 1;
        if c == '\\' {
            if i < chars.len() {
                s.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if c == quote {
            break;
        }
    }
    (s, i)
}

/// Rainbow colour for a bracket at nesting `depth`, cycling through four hues.
pub fn bracket_color(depth: u16) -> Color {
    let cycle = crate::theme::BRACKETS;
    cycle[(depth as usize) % cycle.len()]
}

/// Style for a token class, readable on a dark background.
pub fn class_style(class: Class) -> Style {
    match class {
        Class::Keyword => Style::default().fg(Color::Magenta),
        Class::Str => Style::default().fg(Color::Green),
        Class::Number => Style::default().fg(Color::Cyan),
        Class::Comment => Style::default().fg(Color::DarkGray),
        Class::Function => Style::default().fg(crate::theme::FUNCTION),
        Class::Type => Style::default().fg(Color::Yellow),
        Class::Punct => Style::default().fg(Color::Gray),
        Class::Bracket(depth) => Style::default().fg(bracket_color(depth)),
        Class::Unmatched => Style::default().fg(Color::Red),
        Class::Text => Style::default(),
    }
}

/// Finds the bracket that matches the one at `(line, col)`, searching within a
/// bounded window so it never stalls. Matches by nesting of the same bracket
/// type only (strings and comments are not skipped). Returns the pair position.
pub fn match_bracket(lines: &[String], line: usize, col: usize) -> Option<(usize, usize)> {
    const WINDOW: usize = 2000;
    let row = lines.get(line)?;
    let here = row.chars().nth(col)?;
    if is_open(here) {
        let (open, close) = (here, closer_for(here));
        let mut depth = 0i32;
        let end_line = (line + WINDOW).min(lines.len().saturating_sub(1));
        for l in line..=end_line {
            let rc: Vec<char> = lines[l].chars().collect();
            let start = if l == line { col } else { 0 };
            for (c, &chr) in rc.iter().enumerate().skip(start) {
                if chr == open {
                    depth += 1;
                } else if chr == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((l, c));
                    }
                }
            }
        }
        None
    } else if is_close(here) {
        let (open, close) = (opener_for(here), here);
        let mut depth = 0i32;
        let start_line = line.saturating_sub(WINDOW);
        let mut l = line as isize;
        while l >= start_line as isize {
            let rc: Vec<char> = lines[l as usize].chars().collect();
            let mut c = if l as usize == line { col as isize } else { rc.len() as isize - 1 };
            while c >= 0 {
                let chr = rc[c as usize];
                if chr == close {
                    depth += 1;
                } else if chr == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some((l as usize, c as usize));
                    }
                }
                c -= 1;
            }
            l -= 1;
        }
        None
    } else {
        None
    }
}

/// Incremental, cached highlighter over a buffer's lines. The cache holds the
/// segments and end state of each already-computed line; it is invalidated from
/// the first edited line downward so only visible, un-cached lines are lexed.
pub struct Highlighter {
    lang: Lang,
    cache: Vec<(Vec<Seg>, State)>,
}

impl Highlighter {
    pub fn new(lang: Lang) -> Self {
        Self { lang, cache: Vec::new() }
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// Drops cached lines from `line` down (call after an edit at `line`).
    pub fn invalidate_from(&mut self, line: usize) {
        if line < self.cache.len() {
            self.cache.truncate(line);
        }
    }

    /// Lexes forward until line `upto` (inclusive) is cached.
    pub fn ensure(&mut self, lines: &[String], upto: usize) {
        let target = (upto + 1).min(lines.len());
        while self.cache.len() < target {
            let idx = self.cache.len();
            let state = if idx == 0 { State::default() } else { self.cache[idx - 1].1 };
            let (segs, end) = highlight_line(&lines[idx], self.lang, state);
            self.cache.push((segs, end));
        }
    }

    /// Cached segments for `line`, or empty when not yet computed.
    pub fn segs(&self, line: usize) -> &[Seg] {
        self.cache.get(line).map_or(&[], |c| c.0.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(line: &str, lang: Lang) -> Vec<(String, Class)> {
        highlight_line(line, lang, State::default()).0.into_iter().map(|s| (s.text, s.class)).collect()
    }
    fn class_of<'a>(segs: &'a [(String, Class)], text: &str) -> Option<Class> {
        segs.iter().find(|(t, _)| t == text).map(|(_, c)| *c)
    }

    #[test]
    fn python_keyword_function_and_comment() {
        let segs = classes("def foo():  # hi", Lang::Python);
        assert_eq!(class_of(&segs, "def"), Some(Class::Keyword));
        assert_eq!(class_of(&segs, "foo"), Some(Class::Function));
        assert!(segs.iter().any(|(t, c)| t.contains("# hi") && *c == Class::Comment));
    }

    #[test]
    fn rust_keyword_string_and_comment() {
        let segs = classes(r#"let x = "hi"; // c"#, Lang::Rust);
        assert_eq!(class_of(&segs, "let"), Some(Class::Keyword));
        assert_eq!(class_of(&segs, "\"hi\""), Some(Class::Str));
        assert!(segs.iter().any(|(t, c)| t == "// c" && *c == Class::Comment));
    }

    #[test]
    fn number_is_classified() {
        let segs = classes("x = 42", Lang::Python);
        assert_eq!(class_of(&segs, "42"), Some(Class::Number));
    }

    #[test]
    fn bracket_inside_a_string_is_ignored() {
        let segs = classes(r#""(" "#, Lang::Rust);
        // The "(" is part of the string, never a Bracket token.
        assert_eq!(class_of(&segs, "\"(\""), Some(Class::Str));
        assert!(!segs.iter().any(|(_, c)| matches!(c, Class::Bracket(_))));
    }

    #[test]
    fn rainbow_brackets_nest_by_depth() {
        let segs = classes("([{}])", Lang::Rust);
        let depths: Vec<Class> = segs.iter().map(|(_, c)| *c).collect();
        assert_eq!(
            depths,
            vec![
                Class::Bracket(0),
                Class::Bracket(1),
                Class::Bracket(2),
                Class::Bracket(2),
                Class::Bracket(1),
                Class::Bracket(0),
            ]
        );
    }

    #[test]
    fn unbalanced_closing_bracket_is_flagged() {
        let segs = classes(")", Lang::Rust);
        assert_eq!(segs, vec![(")".to_string(), Class::Unmatched)]);
    }

    #[test]
    fn matching_bracket_is_found_across_lines() {
        let lines: Vec<String> = vec!["foo(".into(), "    bar,".into(), ")".into()];
        // Open paren at line 0 col 3 pairs with the close at line 2 col 0.
        assert_eq!(match_bracket(&lines, 0, 3), Some((2, 0)));
        // And the reverse: the close pairs back to the open.
        assert_eq!(match_bracket(&lines, 2, 0), Some((0, 3)));
        // A non-bracket position returns None.
        assert_eq!(match_bracket(&lines, 0, 0), None);
    }

    #[test]
    fn multiline_python_string_carries_state() {
        let (_, state) = highlight_line(r#"x = """start"#, Lang::Python, State::default());
        assert_eq!(state.ml_string, Some(MlString::TripleDouble));
        let (segs, state2) = highlight_line(r#"still string""" done"#, Lang::Python, state);
        assert_eq!(segs[0].class, Class::Str);
        assert_eq!(state2.ml_string, None);
    }

    #[test]
    fn block_comment_carries_across_lines() {
        let (_, s1) = highlight_line("code /* open", Lang::Rust, State::default());
        assert!(s1.block_comment);
        let (segs, s2) = highlight_line("still */ code", Lang::Rust, s1);
        assert_eq!(segs[0].class, Class::Comment);
        assert!(!s2.block_comment);
    }

    #[test]
    fn highlighting_never_changes_the_text() {
        let samples = [
            (r#"def f(x): return "a(b)" + 1  # c"#, Lang::Python),
            (r#"fn main() { let s = "x\"y"; } // z"#, Lang::Rust),
            ("SELECT * FROM t WHERE a = 1 -- note", Lang::Sql),
            ("const a = `tpl ${x}`;", Lang::JsTs),
        ];
        for (line, lang) in samples {
            let joined: String = highlight_line(line, lang, State::default()).0.into_iter().map(|s| s.text).collect();
            assert_eq!(joined, line, "lexer must preserve the text for {lang:?}");
        }
    }

    #[test]
    #[ignore = "timing-sensitive; run in release with --ignored"]
    fn highlighting_1000_lines_is_fast() {
        let line = r#"    let result = compute(foo, "bar", 42) + other[idx]; // comment"#.to_string();
        let lines: Vec<String> = std::iter::repeat_n(line, 1000).collect();
        let start = std::time::Instant::now();
        let mut hl = Highlighter::new(Lang::Rust);
        hl.ensure(&lines, 999);
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 50, "1000 lines took {elapsed:?}");
    }
}
