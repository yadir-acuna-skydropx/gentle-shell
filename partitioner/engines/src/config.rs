//! `~/.config/gentle-partitioner/engines.toml` — non-secret engine settings.
//!
//! Only the explainer model override and the default output language live here;
//! no secret ever touches this file. Parsed by hand (same minimal `key =
//! "value"` form as the apps config) to avoid a TOML dependency.
use std::path::{Path, PathBuf};

/// Settings for the explainer engine, read from `[explain]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainConfig {
    /// Explicit model id override; when absent the model is resolved at runtime.
    pub model: Option<String>,
    /// Default output language for `explain` (the question's language wins for
    /// `ask`). Defaults to `"es"`.
    pub lang: String,
}

impl Default for ExplainConfig {
    fn default() -> Self {
        Self { model: None, lang: "es".to_string() }
    }
}

/// Directory holding `engines.toml`: `<home>/.config/gentle-partitioner`.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("gentle-partitioner")
}

/// Path to `engines.toml`.
pub fn config_path(home: &Path) -> PathBuf {
    config_dir(home).join("engines.toml")
}

/// Reads the explainer config. A missing file yields the defaults. The
/// `GENTLE_ENGINES_EXPLAIN_MODEL` env overrides the file model (tests).
pub fn load(home: &Path) -> ExplainConfig {
    let mut cfg = match std::fs::read_to_string(config_path(home)) {
        Ok(t) => parse(&t),
        Err(_) => ExplainConfig::default(),
    };
    if let Ok(m) = std::env::var("GENTLE_ENGINES_EXPLAIN_MODEL") {
        if !m.trim().is_empty() {
            cfg.model = Some(m);
        }
    }
    cfg
}

/// Parses the minimal `[explain]` table we support.
pub fn parse(text: &str) -> ExplainConfig {
    let mut cfg = ExplainConfig::default();
    let mut in_explain = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_explain = line == "[explain]";
            continue;
        }
        if !in_explain {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let Some(value) = unquote(value.trim()) else { continue };
        match key.trim() {
            "model" => cfg.model = Some(value),
            "lang" => cfg.lang = value,
            _ => {}
        }
    }
    cfg
}

fn unquote(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    Some(value[1..value.len() - 1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let cfg = ExplainConfig::default();
        assert_eq!(cfg.lang, "es");
        assert_eq!(cfg.model, None);
    }

    #[test]
    fn parses_explain_table() {
        let cfg = parse("[explain]\nmodel = \"claude-haiku-4-5\"\nlang = \"en\"\n");
        assert_eq!(cfg.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(cfg.lang, "en");
    }

    #[test]
    fn ignores_other_tables() {
        let cfg = parse("[other]\nmodel = \"nope\"\n");
        assert_eq!(cfg.model, None);
    }
}
