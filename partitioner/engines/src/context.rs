//! Capping and prompt assembly for the explainer (S17).
//!
//! The selection is capped to [`MAX_SELECTION_LINES`] lines or
//! [`MAX_SELECTION_BYTES`] bytes; the surrounding context to
//! [`MAX_CONTEXT_LINES`] lines each side and [`MAX_CONTEXT_BYTES`] total. The
//! prompt states exactly what was truncated so the model is not misled.
/// Max lines of the selection sent to the model.
pub const MAX_SELECTION_LINES: usize = 400;
/// Max bytes of the selection sent to the model (16 KiB).
pub const MAX_SELECTION_BYTES: usize = 16 * 1024;
/// Max lines of context kept on each side of the selection.
pub const MAX_CONTEXT_LINES: usize = 40;
/// Max bytes of context in total (before + after), 8 KiB.
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024;

/// A piece of text after capping, with a note on what (if anything) was cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capped {
    pub text: String,
    pub truncated: bool,
}

/// Truncates `text` to at most `max_lines` lines, then to at most `max_bytes`
/// bytes on a char boundary. Reports whether anything was removed.
pub fn cap(text: &str, max_lines: usize, max_bytes: usize) -> Capped {
    let mut truncated = false;
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        truncated = true;
    }
    let mut out = lines.join("\n");
    if out.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        truncated = true;
    }
    Capped { text: out, truncated }
}

/// The fully capped inputs for one explain request plus human-readable notes on
/// what was truncated (empty when nothing was cut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedContext {
    pub selection: String,
    pub before: String,
    pub after: String,
    pub notes: Vec<String>,
}

/// Caps the selection and both context sides, splitting the 8 KiB context
/// budget evenly. The `before` text keeps its *last* lines (closest to the
/// selection); `after` keeps its *first* lines.
pub fn cap_context(selection: &str, before: &str, after: &str) -> CappedContext {
    let sel = cap(selection, MAX_SELECTION_LINES, MAX_SELECTION_BYTES);

    let before_tail = tail_lines(before, MAX_CONTEXT_LINES);
    let after_head = head_lines(after, MAX_CONTEXT_LINES);
    let half = MAX_CONTEXT_BYTES / 2;
    let b = cap(&before_tail, MAX_CONTEXT_LINES, half);
    let a = cap(&after_head, MAX_CONTEXT_LINES, half);

    let mut notes = Vec::new();
    if sel.truncated {
        notes.push(format!(
            "the selection was truncated to the first {MAX_SELECTION_LINES} lines / {} KiB",
            MAX_SELECTION_BYTES / 1024
        ));
    }
    let before_cut = b.truncated || lines_count(before) > MAX_CONTEXT_LINES;
    let after_cut = a.truncated || lines_count(after) > MAX_CONTEXT_LINES;
    if before_cut || after_cut {
        notes.push(format!("surrounding context was limited to about {MAX_CONTEXT_LINES} lines per side"));
    }

    CappedContext { selection: sel.text, before: b.text, after: a.text, notes }
}

fn lines_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn head_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Heuristic language pick: Spanish when the text has clear Spanish markers,
/// else `default`. Used so `ask` answers in the question's language.
pub fn detect_lang(text: &str, default: &str) -> String {
    let lower = text.to_lowercase();
    let has_accent = lower.chars().any(|c| matches!(c, 'ñ' | 'á' | 'é' | 'í' | 'ó' | 'ú' | '¿' | '¡'));
    let words = [
        " qué", "¿", "por qué", "cómo", "cuál", "explica", "explicá", "para qué", "porque", " el ", " la ",
        " los ", " las ", " un ", " una ", " esto ", " código", "función",
    ];
    let hit = words.iter().any(|w| lower.contains(w));
    if has_accent || hit { "es".to_string() } else { default.to_string() }
}

/// The stable system prompt (cacheable). Instructs a concise, structured
/// explanation in `lang`.
pub fn system_prompt(lang: &str) -> String {
    let language = match lang {
        "es" => "español",
        "en" => "English",
        other => other,
    };
    format!(
        "You are a concise code explainer embedded in a terminal IDE. Explain the selected code \
clearly and briefly. Always answer in {language}. Structure every explanation as:\n\
1. Purpose — what the code is for, in one or two sentences.\n\
2. How it works — the key steps, in order.\n\
3. Risks / edge cases — bugs, unsafe assumptions, failure modes.\n\
4. Suggestions — concrete improvements, if any.\n\
Keep code identifiers in their original language. Do not invent behavior you cannot see; \
if context was truncated, reason only about what is shown."
    )
}

/// Builds the first user message for an explain request, embedding the capped
/// selection and context and stating what was truncated.
pub fn build_explain_prompt(path: &str, language: &str, ctx: &CappedContext) -> String {
    let mut s = String::new();
    s.push_str(&format!("File: {path}\nLanguage: {language}\n"));
    if !ctx.notes.is_empty() {
        s.push_str("Note: ");
        s.push_str(&ctx.notes.join("; "));
        s.push_str(".\n");
    }
    s.push('\n');
    if !ctx.before.is_empty() {
        s.push_str("Context before the selection:\n```\n");
        s.push_str(&ctx.before);
        s.push_str("\n```\n\n");
    }
    s.push_str("Selected code to explain:\n```\n");
    s.push_str(&ctx.selection);
    s.push_str("\n```\n");
    if !ctx.after.is_empty() {
        s.push_str("\nContext after the selection:\n```\n");
        s.push_str(&ctx.after);
        s.push_str("\n```\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_truncates_by_lines_then_bytes() {
        let text = (0..1000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let c = cap(&text, MAX_SELECTION_LINES, MAX_SELECTION_BYTES);
        assert!(c.truncated);
        assert_eq!(c.text.lines().count(), MAX_SELECTION_LINES);

        let big = "x".repeat(MAX_SELECTION_BYTES + 100);
        let c2 = cap(&big, MAX_SELECTION_LINES, MAX_SELECTION_BYTES);
        assert!(c2.truncated);
        assert!(c2.text.len() <= MAX_SELECTION_BYTES);
    }

    #[test]
    fn cap_keeps_short_text_whole() {
        let c = cap("one\ntwo", MAX_SELECTION_LINES, MAX_SELECTION_BYTES);
        assert!(!c.truncated);
        assert_eq!(c.text, "one\ntwo");
    }

    #[test]
    fn context_keeps_nearest_lines_and_notes_truncation() {
        let before = (0..100).map(|i| format!("b{i}")).collect::<Vec<_>>().join("\n");
        let after = (0..100).map(|i| format!("a{i}")).collect::<Vec<_>>().join("\n");
        let ctx = cap_context("sel", &before, &after);
        assert_eq!(ctx.selection, "sel");
        // before keeps the LAST 40 lines (closest to the selection)
        assert_eq!(ctx.before.lines().count(), MAX_CONTEXT_LINES);
        assert!(ctx.before.contains("b99"));
        assert!(!ctx.before.contains("b0\n") && !ctx.before.starts_with("b0"));
        // after keeps the FIRST 40 lines
        assert!(ctx.after.contains("a0"));
        assert!(!ctx.after.contains("a99"));
        assert!(ctx.notes.iter().any(|n| n.contains("per side")));
    }

    #[test]
    fn detect_lang_spanish_vs_default() {
        assert_eq!(detect_lang("¿qué hace esto?", "en"), "es");
        assert_eq!(detect_lang("explica la función", "en"), "es");
        assert_eq!(detect_lang("what does this do?", "es"), "es"); // default es
        assert_eq!(detect_lang("what does this do?", "en"), "en");
    }

    #[test]
    fn prompt_embeds_truncation_note() {
        let ctx = CappedContext {
            selection: "fn x() {}".into(),
            before: "// b".into(),
            after: String::new(),
            notes: vec!["the selection was truncated".into()],
        };
        let p = build_explain_prompt("src/a.rs", "rust", &ctx);
        assert!(p.contains("File: src/a.rs"));
        assert!(p.contains("truncated"));
        assert!(p.contains("fn x() {}"));
        assert!(p.contains("// b"));
    }

    #[test]
    fn system_prompt_sets_language_and_structure() {
        let es = system_prompt("es");
        assert!(es.contains("español"));
        assert!(es.contains("Purpose"));
        let en = system_prompt("en");
        assert!(en.contains("English"));
    }
}
