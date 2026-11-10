//! Deterministic n-gram prediction engine (S9). No model weights, no network:
//! unigram/bigram/trigram continuation counts with backoff, plus a
//! frequency-ordered prefix index for completing the current word. Two
//! independent models — `chat` and `code` — share this code.
//!
//! The engine also carries the hand-written JSON helpers used by the local
//! predict server and the seeding helpers (repo text for `code`, gentle-shell's
//! shared prompt history for `chat`).
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Which model a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictMode {
    Chat,
    Code,
}

/// Total n-gram entries kept before count-1 pruning kicks in.
const DEFAULT_CAP: usize = 200_000;
/// Hard cap on a returned suggestion, in characters.
const MAX_SUGGESTION: usize = 60;
/// At least this many characters must be typed before completing a word.
const MIN_PREFIX: usize = 2;
/// A candidate below this count is treated as noise and never suggested.
const MIN_COUNT: u32 = 2;
/// Next-word prediction stops after this many words.
const MAX_NEXT_WORDS: usize = 3;
/// Only the tail of the context is tokenized, bounding predict cost on huge input.
const CONTEXT_CHARS: usize = 200;

fn is_word_char(c: char) -> bool {
    // Unicode letters (Spanish accents and ñ included), digits and underscore.
    c.is_alphanumeric() || c == '_'
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Punct,
    Newline,
}

/// One token: `key` is the lowercase matching form, `surface` the original
/// casing used when rendering a suggestion.
struct Tok {
    key: String,
    surface: String,
    kind: Kind,
}

/// Splits text into word tokens (letters/digits/`_`), single punctuation tokens
/// and newline sentinels. Other whitespace is dropped — it is a separator, not a
/// token — and spacing is rebuilt heuristically when a suggestion is rendered.
fn tokenize(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Tok>| {
        if !buf.is_empty() {
            out.push(Tok { key: buf.to_lowercase(), surface: buf.clone(), kind: Kind::Word });
            buf.clear();
        }
    };
    for c in text.chars() {
        if is_word_char(c) {
            buf.push(c);
        } else {
            flush(&mut buf, &mut out);
            if c == '\n' {
                out.push(Tok { key: "\n".into(), surface: "\n".into(), kind: Kind::Newline });
            } else if !c.is_whitespace() {
                let s = c.to_string();
                out.push(Tok { key: s.clone(), surface: s, kind: Kind::Punct });
            }
        }
    }
    flush(&mut buf, &mut out);
    out
}

/// A token key is punctuation when it is a single non-word, non-newline char.
fn is_punct_key(k: &str) -> bool {
    let mut it = k.chars();
    matches!((it.next(), it.next()), (Some(c), None) if !is_word_char(c) && c != '\n')
}

/// Picks the highest-count continuation; ties break on the smallest key so the
/// engine is deterministic.
fn pick_best(m: &HashMap<String, u32>) -> Option<(String, u32)> {
    let mut best: Option<(&String, u32)> = None;
    for (k, &c) in m {
        match best {
            Some((bk, bc)) if bc > c || (bc == c && bk <= k) => {}
            _ => best = Some((k, c)),
        }
    }
    best.map(|(k, c)| (k.clone(), c))
}

fn cap_suggestion(s: String) -> String {
    if s.chars().count() <= MAX_SUGGESTION {
        s
    } else {
        s.chars().take(MAX_SUGGESTION).collect()
    }
}

fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        s.to_string()
    } else {
        s.chars().skip(total - n).collect()
    }
}

/// One mode's counts: unigram, bigram and trigram continuation tables plus the
/// frequency-ordered word index used for completion.
#[derive(Default)]
struct Model {
    unigram: HashMap<String, u32>,
    bigram: HashMap<String, HashMap<String, u32>>,
    trigram: HashMap<(String, String), HashMap<String, u32>>,
    /// Lowercase word -> count, ordered so a prefix range is cheap.
    word_counts: BTreeMap<String, u32>,
    /// Lowercase token -> representative surface casing.
    surface: HashMap<String, String>,
    cap: usize,
}

impl Model {
    fn with_cap(cap: usize) -> Self {
        Self { cap, ..Default::default() }
    }

    fn learn(&mut self, text: &str) {
        let toks = tokenize(text);
        for (i, t) in toks.iter().enumerate() {
            *self.unigram.entry(t.key.clone()).or_insert(0) += 1;
            self.surface.entry(t.key.clone()).or_insert_with(|| t.surface.clone());
            if t.kind == Kind::Word {
                *self.word_counts.entry(t.key.clone()).or_insert(0) += 1;
            }
            if i >= 1 {
                let w1 = toks[i - 1].key.clone();
                *self.bigram.entry(w1).or_default().entry(t.key.clone()).or_insert(0) += 1;
            }
            if i >= 2 {
                let w1 = toks[i - 2].key.clone();
                let w2 = toks[i - 1].key.clone();
                *self.trigram.entry((w1, w2)).or_default().entry(t.key.clone()).or_insert(0) += 1;
            }
        }
        if self.entry_count() > self.cap {
            self.prune();
        }
    }

    fn entry_count(&self) -> usize {
        self.unigram.len()
            + self.bigram.values().map(HashMap::len).sum::<usize>()
            + self.trigram.values().map(HashMap::len).sum::<usize>()
    }

    /// Simple bound: drop count-1 entries when over the cap.
    fn prune(&mut self) {
        self.unigram.retain(|_, c| *c > 1);
        self.word_counts.retain(|_, c| *c > 1);
        for m in self.bigram.values_mut() {
            m.retain(|_, c| *c > 1);
        }
        self.bigram.retain(|_, m| !m.is_empty());
        for m in self.trigram.values_mut() {
            m.retain(|_, c| *c > 1);
        }
        self.trigram.retain(|_, m| !m.is_empty());
        let keep: HashSet<String> = self.unigram.keys().cloned().collect();
        self.surface.retain(|k, _| keep.contains(k));
    }

    fn predict(&self, text: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        let tail = tail_chars(text, CONTEXT_CHARS);
        let last = tail.chars().last()?;
        if is_word_char(last) {
            self.complete(&tail)
        } else {
            self.next_words(&tail)
        }
    }

    /// Completes the partial trailing word with the most frequent longer word
    /// that shares its (case-insensitive) prefix.
    fn complete(&self, tail: &str) -> Option<String> {
        let chars: Vec<char> = tail.chars().collect();
        let mut start = chars.len();
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let prefix: String = chars[start..].iter().collect();
        if prefix.chars().count() < MIN_PREFIX {
            return None;
        }
        let plower = prefix.to_lowercase();
        let prefix_len = plower.chars().count();
        let mut best: Option<(&String, u32)> = None;
        for (w, &c) in self.word_counts.range(plower.clone()..) {
            if !w.starts_with(&plower) {
                break;
            }
            if w.chars().count() <= prefix_len || c < MIN_COUNT {
                continue;
            }
            match best {
                Some((_, bc)) if bc >= c => {}
                _ => best = Some((w, c)),
            }
        }
        let (wkey, _) = best?;
        let surf = self.surface.get(wkey).cloned().unwrap_or_else(|| wkey.clone());
        let suffix: String = surf.chars().skip(prefix_len).collect();
        if suffix.is_empty() {
            return None;
        }
        Some(cap_suggestion(suffix))
    }

    /// Predicts up to three next words by trigram → bigram → unigram backoff,
    /// stopping at a line end, a below-threshold count or the 60-char cap.
    fn next_words(&self, tail: &str) -> Option<String> {
        let toks = tokenize(tail);
        let n = toks.len();
        let mut ctx1 = (n >= 2).then(|| toks[n - 2].key.clone());
        let mut ctx2 = (n >= 1).then(|| toks[n - 1].key.clone());
        let ends_ws = tail.chars().last().is_none_or(char::is_whitespace);

        let mut out = String::new();
        let mut words = 0;
        let mut first = true;
        while words < MAX_NEXT_WORDS {
            let Some((key, count)) = self.best_next(ctx1.as_deref(), ctx2.as_deref()) else { break };
            if count < MIN_COUNT || key == "\n" {
                break;
            }
            let is_punct = is_punct_key(&key);
            let surf = self.surface.get(&key).cloned().unwrap_or_else(|| key.clone());
            let mut piece = String::new();
            if first {
                if !ends_ws && !is_punct {
                    piece.push(' ');
                }
            } else if !is_punct {
                piece.push(' ');
            }
            piece.push_str(&surf);
            if out.chars().count() + piece.chars().count() > MAX_SUGGESTION {
                break;
            }
            out.push_str(&piece);
            first = false;
            if !is_punct {
                words += 1;
            }
            ctx1 = ctx2.take();
            ctx2 = Some(key);
        }
        if out.trim().is_empty() {
            return None;
        }
        Some(cap_suggestion(out))
    }

    fn best_next(&self, w1: Option<&str>, w2: Option<&str>) -> Option<(String, u32)> {
        if let (Some(a), Some(b)) = (w1, w2) {
            if let Some(m) = self.trigram.get(&(a.to_string(), b.to_string())) {
                if let Some(x) = pick_best(m) {
                    return Some(x);
                }
            }
        }
        if let Some(b) = w2 {
            if let Some(m) = self.bigram.get(b) {
                if let Some(x) = pick_best(m) {
                    return Some(x);
                }
            }
        }
        self.top_word()
    }

    /// Unigram fallback: the most frequent word (deterministic on ties).
    fn top_word(&self) -> Option<(String, u32)> {
        let mut best: Option<(&String, u32)> = None;
        for (k, &c) in &self.word_counts {
            if c < MIN_COUNT {
                continue;
            }
            match best {
                Some((bk, bc)) if bc > c || (bc == c && bk <= k) => {}
                _ => best = Some((k, c)),
            }
        }
        best.map(|(k, c)| (k.clone(), c))
    }
}

/// The prediction engine: one model per mode.
pub struct Engine {
    chat: Model,
    code: Model,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_CAP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self { chat: Model::with_cap(cap), code: Model::with_cap(cap) }
    }

    fn model(&self, mode: PredictMode) -> &Model {
        match mode {
            PredictMode::Chat => &self.chat,
            PredictMode::Code => &self.code,
        }
    }

    fn model_mut(&mut self, mode: PredictMode) -> &mut Model {
        match mode {
            PredictMode::Chat => &mut self.chat,
            PredictMode::Code => &mut self.code,
        }
    }

    pub fn learn(&mut self, text: &str, mode: PredictMode) {
        self.model_mut(mode).learn(text);
    }

    /// Returns ONLY the suffix to insert at the cursor, or `None`.
    pub fn predict(&self, text: &str, mode: PredictMode) -> Option<String> {
        self.model(mode).predict(text)
    }

    /// Total n-gram entries in a model (for the memory-cap tests).
    pub fn len(&self, mode: PredictMode) -> usize {
        self.model(mode).entry_count()
    }

    pub fn is_empty(&self, mode: PredictMode) -> bool {
        self.len(mode) == 0
    }
}

// ---------------------------------------------------------------------------
// Hand-written JSON (the two request/response shapes only)
// ---------------------------------------------------------------------------

/// Escapes a string for a JSON value: quotes, backslashes and control
/// characters (named escapes where defined, `\uXXXX` otherwise). Non-ASCII
/// text (Spanish accents, ñ) passes through literally.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while matches!(chars.get(*i), Some(' ' | '\t' | '\n' | '\r')) {
        *i += 1;
    }
}

fn consume_lit(chars: &[char], i: &mut usize, lit: &str) -> Option<()> {
    for ch in lit.chars() {
        if chars.get(*i) != Some(&ch) {
            return None;
        }
        *i += 1;
    }
    Some(())
}

fn parse_hex4(chars: &[char], i: &mut usize) -> Option<u32> {
    let mut code = 0u32;
    for _ in 0..4 {
        let h = chars.get(*i)?;
        *i += 1;
        code = code * 16 + h.to_digit(16)?;
    }
    Some(code)
}

fn parse_json_string(chars: &[char], i: &mut usize) -> Option<String> {
    if chars.get(*i) != Some(&'"') {
        return None;
    }
    *i += 1;
    let mut s = String::new();
    while let Some(&c) = chars.get(*i) {
        *i += 1;
        match c {
            '"' => return Some(s),
            '\\' => {
                let e = *chars.get(*i)?;
                *i += 1;
                match e {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{08}'),
                    'f' => s.push('\u{0c}'),
                    'u' => {
                        let code = parse_hex4(chars, i)?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            // High surrogate: a low surrogate must follow.
                            if chars.get(*i) == Some(&'\\') && chars.get(*i + 1) == Some(&'u') {
                                *i += 2;
                                let low = parse_hex4(chars, i)?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return None;
                                }
                                let cp = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                s.push(char::from_u32(cp)?);
                            } else {
                                return None;
                            }
                        } else {
                            s.push(char::from_u32(code)?);
                        }
                    }
                    _ => return None,
                }
            }
            _ => s.push(c),
        }
    }
    None
}

/// Parses a flat JSON object into its string-valued keys. Non-string values
/// (numbers, booleans, null) are accepted and skipped so history lines like
/// `{"v":1,"text":"...","ts":123}` work; nested objects/arrays and any
/// structural garbage yield `None` (a 400 for the server).
pub fn parse_json_object(input: &str) -> Option<Vec<(String, String)>> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    skip_ws(&chars, &mut i);
    if chars.get(i) != Some(&'{') {
        return None;
    }
    i += 1;
    skip_ws(&chars, &mut i);
    let mut out = Vec::new();
    if chars.get(i) == Some(&'}') {
        i += 1;
        skip_ws(&chars, &mut i);
        return (i == chars.len()).then_some(out);
    }
    loop {
        skip_ws(&chars, &mut i);
        let key = parse_json_string(&chars, &mut i)?;
        skip_ws(&chars, &mut i);
        if chars.get(i) != Some(&':') {
            return None;
        }
        i += 1;
        skip_ws(&chars, &mut i);
        if let Some(v) = parse_json_value(&chars, &mut i)? {
            out.push((key, v));
        }
        skip_ws(&chars, &mut i);
        match chars.get(i) {
            Some(',') => {
                i += 1;
                continue;
            }
            Some('}') => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    skip_ws(&chars, &mut i);
    (i == chars.len()).then_some(out)
}

/// Parses one JSON value. `Some(Some(s))` for a string, `Some(None)` for an
/// accepted-and-ignored primitive, `None` for anything unsupported.
fn parse_json_value(chars: &[char], i: &mut usize) -> Option<Option<String>> {
    match chars.get(*i)? {
        '"' => Some(Some(parse_json_string(chars, i)?)),
        't' => consume_lit(chars, i, "true").map(|_| None),
        'f' => consume_lit(chars, i, "false").map(|_| None),
        'n' => consume_lit(chars, i, "null").map(|_| None),
        c if *c == '-' || c.is_ascii_digit() => {
            let start = *i;
            while matches!(chars.get(*i), Some(d) if *d == '-' || *d == '+' || *d == '.' || *d == 'e' || *d == 'E' || d.is_ascii_digit()) {
                *i += 1;
            }
            (*i > start).then_some(None)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

const MAX_SEED_FILE: u64 = 256 * 1024;
const MAX_SEED_TOTAL: usize = 20 * 1024 * 1024;

/// Seeds the `code` model from the repo's tracked text files via `git ls-files`.
/// Binary files and files over 256 KiB are skipped; the total is capped at
/// 20 MiB. Intended to run on a background thread at startup.
pub fn seed_code_repo(engine: &RwLock<Engine>, root: &Path) {
    let Some(files) = git_ls_files(root) else { return };
    let mut total = 0usize;
    for rel in files {
        let path = root.join(&rel);
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() || meta.len() > MAX_SEED_FILE {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue; // binary
        }
        let Ok(text) = String::from_utf8(bytes) else { continue };
        total += text.len();
        if total > MAX_SEED_TOTAL {
            break;
        }
        if let Ok(mut e) = engine.write() {
            e.learn(&text, PredictMode::Code);
        }
    }
}

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git").arg("ls-files").current_dir(root).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

/// gentle-shell's shared prompt-history store root, when present. The format is
/// JSONL files `{"v":1,"text":"...","ts":...}` under
/// `<agent dir>/history/{history-global.jsonl, projects/<hash>/*.jsonl}`
/// (extensions/history/index.ts:127,130,1326 and store.ts:65,74,183-218).
/// The bundled gentle-shell uses `~/.gentle-shell/agent`; plain Pi uses `~/.pi/agent`.
pub fn history_store_roots(home: &Path) -> Vec<PathBuf> {
    [home.join(".gentle-shell/agent/history"), home.join(".pi/agent/history")]
        .into_iter()
        .filter(|root| root.is_dir())
        .collect()
}

/// Reads every prompt's `text` from a history store root, read-only.
pub fn read_history_prompts(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_jsonl(&root.join("history-global.jsonl"), &mut out);
    let projects = root.join("projects");
    if let Ok(rd) = std::fs::read_dir(&projects) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                for f in rd2.flatten() {
                    let p = f.path();
                    if p.extension().is_some_and(|x| x == "jsonl") {
                        collect_jsonl(&p, &mut out);
                    }
                }
            }
        }
    }
    out
}

fn collect_jsonl(path: &Path, out: &mut Vec<String>) {
    let Ok(content) = std::fs::read_to_string(path) else { return };
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(fields) = parse_json_object(line.trim()) {
            if let Some((_, text)) = fields.into_iter().find(|(k, _)| k == "text") {
                if !text.trim().is_empty() {
                    out.push(text);
                }
            }
        }
    }
}

/// Seeds the `chat` model from the shared prompt history, if found. Returns the
/// store root it used so the caller can cite it.
pub fn seed_chat_history(engine: &RwLock<Engine>) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let roots = history_store_roots(&home);
    let root = roots.first()?.clone();
    let prompts: Vec<String> = roots.iter().flat_map(|r| read_history_prompts(r)).collect();
    if prompts.is_empty() {
        return None;
    }
    if let Ok(mut e) = engine.write() {
        for p in &prompts {
            e.learn(p, PredictMode::Chat);
        }
    }
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::PredictMode::{Chat, Code};
    use super::*;

    #[test]
    fn completes_the_most_frequent_word_with_accents() {
        let mut e = Engine::new();
        e.learn("configuración configuración configurar", Code);
        // "configuración" (count 2) beats "configurar" (count 1, below MIN_COUNT).
        assert_eq!(e.predict("confi", Code).as_deref(), Some("guración"));
    }

    #[test]
    fn preserves_internal_case_of_the_completed_word() {
        let mut e = Engine::new();
        e.learn("PtyPane PtyPane", Code);
        // Typed prefix stays untouched (only the suffix is returned); the
        // suffix keeps the learned word's casing.
        assert_eq!(e.predict("Pty", Code).as_deref(), Some("Pane"));
        assert_eq!(e.predict("pty", Code).as_deref(), Some("Pane"));
    }

    #[test]
    fn predicts_next_words_via_trigram_backoff_stopping_at_line_end() {
        let mut e = Engine::new();
        e.learn("the quick brown fox\n", Chat);
        e.learn("the quick brown fox\n", Chat);
        e.learn("the quick brown fox\n", Chat);
        // Boundary (trailing space): trigram the|quick->brown, quick|brown->fox,
        // brown|fox->newline stops. No leading space because context ends in ws.
        assert_eq!(e.predict("the quick ", Chat).as_deref(), Some("brown fox"));
    }

    #[test]
    fn never_suggests_on_empty_or_whitespace_only_input() {
        let mut e = Engine::new();
        e.learn("hola hola mundo mundo", Chat);
        assert_eq!(e.predict("", Chat), None);
        assert_eq!(e.predict("   \t ", Chat), None);
    }

    #[test]
    fn never_suggests_when_the_best_count_is_below_two() {
        let mut e = Engine::new();
        e.learn("zzyzx", Code); // single occurrence
        assert_eq!(e.predict("zz", Code), None); // completion count < 2
        e.learn("alpha beta", Chat); // beta continuation count 1
        assert_eq!(e.predict("alpha ", Chat), None); // next-word count < 2
    }

    #[test]
    fn requires_at_least_two_typed_characters_to_complete() {
        let mut e = Engine::new();
        e.learn("alpha alpha", Code);
        assert_eq!(e.predict("a", Code), None);
        assert_eq!(e.predict("al", Code).as_deref(), Some("pha"));
    }

    #[test]
    fn caps_the_suggestion_at_sixty_chars() {
        let mut e = Engine::new();
        let long: String = std::iter::repeat_n('a', 100).collect();
        e.learn(&format!("{long} {long}"), Code);
        let s = e.predict("aaa", Code).unwrap();
        assert_eq!(s.chars().count(), MAX_SUGGESTION);
    }

    #[test]
    fn pruning_bounds_the_vocabulary_at_the_cap() {
        let mut e = Engine::with_cap(16);
        // Many single-occurrence tokens: pruning drops count-1 entries once the
        // cap is crossed, so the model stays bounded instead of growing forever.
        let text: String = (0..200).map(|i| format!("w{i} ")).collect();
        e.learn(&text, Code);
        assert!(e.len(Code) <= 16, "entries={}", e.len(Code));
    }

    #[test]
    fn json_escape_round_trips_spanish_quotes_and_newlines() {
        let original = "configuración \"especial\"\n\ty ñoño\r\u{1}";
        let body = format!("{{\"text\":\"{}\"}}", json_escape(original));
        let fields = parse_json_object(&body).expect("valid json");
        let (_, text) = fields.into_iter().find(|(k, _)| k == "text").unwrap();
        assert_eq!(text, original);
    }

    #[test]
    fn parse_json_object_rejects_garbage_but_skips_numeric_fields() {
        assert!(parse_json_object("not json").is_none());
        assert!(parse_json_object("{\"text\":").is_none());
        assert!(parse_json_object("{\"text\":{}}").is_none());
        // History-style line with numeric v/ts fields parses; only text is kept.
        let fields = parse_json_object("{\"v\":1,\"text\":\"hi\",\"ts\":123}").unwrap();
        assert_eq!(fields, vec![("text".to_string(), "hi".to_string())]);
    }

    #[test]
    fn history_roots_cover_pi_and_bundled_gentle_shell_homes() {
        let home = std::env::temp_dir().join(format!("gentle-predict-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(history_store_roots(&home).is_empty());
        std::fs::create_dir_all(home.join(".gentle-shell/agent/history")).unwrap();
        assert_eq!(history_store_roots(&home), vec![home.join(".gentle-shell/agent/history")]);
        std::fs::create_dir_all(home.join(".pi/agent/history")).unwrap();
        assert_eq!(
            history_store_roots(&home),
            vec![home.join(".gentle-shell/agent/history"), home.join(".pi/agent/history")]
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn reads_text_from_history_jsonl() {
        let d = std::env::temp_dir().join(format!("gentle-predict-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("projects/abc")).unwrap();
        std::fs::write(d.join("history-global.jsonl"), "{\"v\":1,\"text\":\"global prompt\"}\n").unwrap();
        std::fs::write(d.join("projects/abc/s.jsonl"), "{\"v\":1,\"text\":\"configuración\",\"ts\":7}\nbad line\n").unwrap();
        let mut prompts = read_history_prompts(&d);
        prompts.sort();
        assert_eq!(prompts, vec!["configuración".to_string(), "global prompt".to_string()]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[ignore = "timing: run in release with --ignored (predict latency target)"]
    fn predict_is_sub_five_ms_after_seeding_five_megabytes() {
        use std::time::Instant;
        let vocab = ["the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "configuración", "sistema"];
        let mut text = String::with_capacity(5 * 1024 * 1024 + 64);
        let mut n = 0;
        while text.len() < 5 * 1024 * 1024 {
            text.push_str(vocab[n % vocab.len()]);
            text.push(if n % 8 == 7 { '\n' } else { ' ' });
            n += 1;
        }
        let mut e = Engine::new();
        e.learn(&text, Code);
        // Warm, then time a single predict.
        let _ = e.predict("the quick ", Code);
        let start = Instant::now();
        let _ = e.predict("the quick ", Code);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs_f64() * 1000.0 < 5.0, "predict took {elapsed:?}");
    }
}
