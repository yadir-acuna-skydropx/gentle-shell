//! The architecture engine core (S18): KB retrieval, prompt assembly with
//! citations and ephemeral cache control, and report writing. Pure helpers are
//! unit-tested; the Anthropic-backed backend is exercised against the fake
//! server in `tests/arch_http.rs`.
use std::path::{Path, PathBuf};

use crate::anthropic::{Client, Message};
use crate::kb::{self, Index};
use crate::scan::{self, ScanResult};
use crate::tools;

/// Number of KB chunks retrieved into the prompt.
pub const TOP_K: usize = 12;
/// Completion budget for an architecture report.
pub const ARCH_MAX_TOKENS: u32 = 8192;

/// Builds the KB context block with `[n]` citations, one per retrieved chunk.
/// The block is part of the (cacheable) system prompt.
pub fn build_kb_block(index: &Index, chunk_indices: &[usize]) -> String {
    if chunk_indices.is_empty() {
        return "No knowledge-base sources are indexed yet (run `gentle-arch kb update`).".to_string();
    }
    let mut s = String::from("Knowledge base (cite as [n] with the URL):\n");
    for (n, &idx) in chunk_indices.iter().enumerate() {
        if let Some(c) = index.chunks.get(idx) {
            s.push_str(&format!("[{}] {}\n{}\n\n", n + 1, c.url, c.text));
        }
    }
    s
}

/// The citation sources list (markdown) for the report's "sources" section.
pub fn citations(index: &Index, chunk_indices: &[usize]) -> Vec<(usize, String)> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (n, &idx) in chunk_indices.iter().enumerate() {
        if let Some(c) = index.chunks.get(idx) {
            out.push((n + 1, c.url.clone()));
            if !seen.contains(&c.url) {
                seen.push(c.url.clone());
            }
        }
    }
    out
}

/// The shared report structure instruction.
const REPORT_STRUCTURE: &str = "\
Produce a single structured markdown report with these sections, in order:\n\
1. Executive summary.\n\
2. Detected/proposed architecture, including a Mermaid C4 (Context or Container) diagram in a ```mermaid block.\n\
3. Findings grouped by Google Cloud Architecture Framework pillar (operational excellence, security, reliability, cost optimization, performance). For each: severity, evidence as file:line when available, a concrete recommendation, and citations [n].\n\
4. Cost analysis. Use Infracost numbers when provided; otherwise give estimates CLEARLY marked as estimates with their assumptions (e.g. Cloud Composer vs Cloud Scheduler + Cloud Run when DAGs are present).\n\
5. CI/CD automation proposals (deploy pipelines per environment).\n\
6. ADRs in MADR format for the top 3 decisions.\n\
7. Sources — list every [n] citation with its URL.\n\
Cite claims grounded in the knowledge base as [n]. Do not invent file paths or numbers you cannot see.";

/// System prompt for a repo review.
pub fn system_prompt_review(kb_block: &str) -> String {
    format!(
        "You are a senior GCP infrastructure and data architect embedded in a terminal IDE. \
You evaluate repositories (Airflow DAGs, CI, IaC) against best practices, favoring GCP-native, \
cost-aware designs (push compute to BigQuery/Dataflow; weigh Cloud Composer against Cloud Scheduler + Cloud Run + Workflows).\n\n\
{REPORT_STRUCTURE}\n\n{kb_block}"
    )
}

/// System prompt for a design brief. Answers in the brief's language.
pub fn system_prompt_design(kb_block: &str, language: &str) -> String {
    format!(
        "You are a senior GCP infrastructure and data architect. From a project brief you design a \
microservices architecture with data flows, orchestration options and costs, and banking-grade \
security/compliance when relevant (IAM, VPC-SC, CMEK, audit, PII). Answer in {language}. \
If the brief is ambiguous, open with clarifying questions.\n\n\
{REPORT_STRUCTURE}\n\n{kb_block}"
    )
}

/// Builds the review user message from the scan and tool summaries.
pub fn build_review_user(root: &str, scan: &ScanResult, tool_lines: &[String]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Repository: {root}\n\n"));
    s.push_str("Scan manifest:\n");
    s.push_str(&scan.manifest());
    s.push('\n');
    if !tool_lines.is_empty() {
        s.push_str("External tools:\n");
        for l in tool_lines {
            s.push_str(&format!("  {l}\n"));
        }
        s.push('\n');
    }
    s.push_str("Files (redacted):\n");
    for f in &scan.files {
        s.push_str(&format!("\n----- {} [{}] -----\n", f.path, f.category.label()));
        s.push_str(&f.content);
        if !f.content.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// Builds the design user message from the brief.
pub fn build_design_user(brief: &str) -> String {
    format!("Project brief:\n{brief}\n")
}

/// Spanish when the brief has clear Spanish markers, else English. Returns the
/// language code (`"es"` / `"en"`).
pub fn brief_language(brief: &str) -> String {
    crate::context::detect_lang(brief, "en")
}

/// Human display name for a language code, for the design system prompt.
pub fn language_display(code: &str) -> &str {
    match code {
        "es" => "español",
        "en" => "English",
        other => other,
    }
}

/// Picks the next non-overwriting report path under `<root>/.gentle-arch` named
/// `<kind>-<timestamp>.md`, bumping a `-N` suffix when the file already exists.
pub fn report_path(root: &Path, kind: &str, timestamp: &str) -> PathBuf {
    let dir = root.join(".gentle-arch");
    let base = dir.join(format!("{kind}-{timestamp}.md"));
    if !base.exists() {
        return base;
    }
    for n in 1..10_000 {
        let candidate = dir.join(format!("{kind}-{timestamp}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

/// Writes `content` to a fresh report file, creating `.gentle-arch`. Returns the
/// path actually written (never overwriting an existing file).
pub fn write_report(root: &Path, kind: &str, timestamp: &str, content: &str) -> std::io::Result<PathBuf> {
    write_report_at(root, kind, timestamp, None, content)
}

/// Writes `content` to a fresh report file. When `out` is `Some`, that path is
/// used (bumping a `-N` suffix if it already exists); otherwise the default
/// `<root>/.gentle-arch/<kind>-<timestamp>.md`. Never overwrites.
pub fn write_report_at(
    root: &Path,
    kind: &str,
    timestamp: &str,
    out: Option<&Path>,
    content: &str,
) -> std::io::Result<PathBuf> {
    let path = match out {
        Some(o) => fresh_path(o),
        None => report_path(root, kind, timestamp),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Returns `desired` if free, else bumps a `-N` suffix before the extension.
pub fn fresh_path(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }
    let stem = desired.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = desired.extension().map(|e| e.to_string_lossy().to_string());
    let parent = desired.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for n in 1..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    desired.to_path_buf()
}

/// Runs the optional external tools for a review, returning their render lines.
pub fn run_tools(path: &Path, use_tools: bool) -> Vec<String> {
    if !use_tools {
        return Vec::new();
    }
    vec![tools::run_infracost(path).render("Infracost"), tools::run_checkov(path).render("Checkov")]
}

/// Retrieval: the top [`TOP_K`] KB chunks for a review's detected technologies.
pub fn retrieve_for_scan(index: &Index, scan: &ScanResult) -> Vec<usize> {
    let query = scan.detected_technologies().join(" ");
    let query = if query.trim().is_empty() { "gcp architecture framework best practices".to_string() } else { query };
    index.search(&query, TOP_K)
}

/// Retrieval: the top [`TOP_K`] KB chunks for a design brief.
pub fn retrieve_for_brief(index: &Index, brief: &str) -> Vec<usize> {
    index.search(brief, TOP_K)
}

/// A streaming turn result.
pub struct ArchOutput {
    pub report: String,
    pub path: PathBuf,
}

/// The architecture backend: holds the client, KB index and model.
pub struct ArchBackend {
    client: Option<Client>,
    index: Index,
    /// Resolved base model (sonnet line) or the override.
    model_override: Option<String>,
    sonnet_model: String,
    opus_model: String,
}

impl ArchBackend {
    /// Builds from the key store, home (config + KB dir) and API base.
    pub fn new(store: &dyn crate::keystore::KeyStore, home: &Path, base: &str) -> Self {
        let kb_dir = kb::kb_dir(home);
        Self::new_with_kb(store, home, base, &kb_dir)
    }

    /// Builds with an explicit KB directory (tests, to avoid env races).
    pub fn new_with_kb(store: &dyn crate::keystore::KeyStore, home: &Path, base: &str, kb_dir: &Path) -> Self {
        let cfg = crate::config::load_arch(home);
        let index = kb::load(kb_dir);
        let client = match store.load() {
            Ok(Some(key)) if !key.trim().is_empty() => Some(Client::new(key, base)),
            _ => None,
        };
        ArchBackend {
            client,
            index,
            model_override: cfg.model,
            sonnet_model: String::new(),
            opus_model: String::new(),
        }
    }

    /// KB chunk count (for `status`).
    pub fn kb_chunks(&self) -> usize {
        self.index.chunk_count()
    }

    /// Whether a key is configured.
    pub fn has_key(&self) -> bool {
        self.client.is_some()
    }

    /// Resolves and caches the model id for the requested line.
    fn model_for(&mut self, deep: bool) -> Result<String, String> {
        if let Some(m) = self.model_override.clone().filter(|m| !m.trim().is_empty()) {
            return Ok(m);
        }
        let line = if deep { "opus" } else { "sonnet" };
        let cached = if deep { &self.opus_model } else { &self.sonnet_model };
        if !cached.is_empty() {
            return Ok(cached.clone());
        }
        let client = self.client.as_ref().ok_or_else(missing_key_msg)?;
        let id = client.resolve_model(line, None).map_err(|e| e.to_string())?;
        if deep {
            self.opus_model = id.clone();
        } else {
            self.sonnet_model = id.clone();
        }
        Ok(id)
    }

    /// The resolved default (sonnet) model id if known, else empty.
    pub fn default_model(&self) -> String {
        if let Some(m) = &self.model_override {
            return m.clone();
        }
        self.sonnet_model.clone()
    }

    /// Resolves the default model up front (best-effort, for `status`).
    pub fn warm_model(&mut self) {
        let _ = self.model_for(false);
    }

    /// Runs a repo review end to end: scan, tools, retrieval, stream, write.
    /// `out`, when `Some`, is the desired report path (bumped if it exists);
    /// otherwise the default `<root>/.gentle-arch/review-<ts>.md` is used.
    #[allow(clippy::too_many_arguments)]
    pub fn review(
        &mut self,
        root: &Path,
        deep: bool,
        use_tools: bool,
        timestamp: &str,
        out: Option<&Path>,
        sink: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<ArchOutput, String> {
        if self.client.is_none() {
            return Err(missing_key_msg());
        }
        let scan = scan::scan(root);
        let tool_lines = run_tools(root, use_tools);
        let picks = retrieve_for_scan(&self.index, &scan);
        let kb_block = build_kb_block(&self.index, &picks);
        let system = system_prompt_review(&kb_block);
        let user = build_review_user(&root.display().to_string(), &scan, &tool_lines);
        let report = self.stream(&system, &user, deep, sink, cancel)?;
        if cancel() {
            return Err("cancelled".to_string());
        }
        let path = write_report_at(root, "review", timestamp, out, &report).map_err(|e| e.to_string())?;
        Ok(ArchOutput { report, path })
    }

    /// Runs a design end to end: retrieval on the brief, stream, write.
    #[allow(clippy::too_many_arguments)]
    pub fn design(
        &mut self,
        root: &Path,
        brief: &str,
        deep: bool,
        timestamp: &str,
        out: Option<&Path>,
        sink: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<ArchOutput, String> {
        if self.client.is_none() {
            return Err(missing_key_msg());
        }
        let picks = retrieve_for_brief(&self.index, brief);
        let kb_block = build_kb_block(&self.index, &picks);
        let code = brief_language(brief);
        let language = language_display(&code);
        let system = system_prompt_design(&kb_block, language);
        let user = build_design_user(brief);
        let report = self.stream(&system, &user, deep, sink, cancel)?;
        if cancel() {
            return Err("cancelled".to_string());
        }
        let path = write_report_at(root, "design", timestamp, out, &report).map_err(|e| e.to_string())?;
        Ok(ArchOutput { report, path })
    }

    fn stream(
        &mut self,
        system: &str,
        user: &str,
        deep: bool,
        sink: &mut dyn FnMut(&str),
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let model = self.model_for(deep)?;
        let client = self.client.as_ref().ok_or_else(missing_key_msg)?;
        let mut acc = String::new();
        let mut forward = |text: &str| {
            acc.push_str(text);
            sink(text);
        };
        client
            .stream_message_with(system, &[Message::user(user)], &model, ARCH_MAX_TOKENS, &mut forward, cancel)
            .map_err(|e| e.to_string())?;
        Ok(acc)
    }
}

fn missing_key_msg() -> String {
    "no Anthropic API key — run `gentle-arch key set`".to_string()
}

/// A compact UTC timestamp `YYYYMMDDThhmmss` for report file names.
pub fn timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}")
}

/// Howard Hinnant's days-from-epoch → civil date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Adapts [`ArchBackend`] to the engine's [`crate::arch_engine::ArchRunner`]
/// trait: resolves the repo root (empty path = cwd), generates a timestamp, and
/// enables the optional tools (the stdio engine has no `--no-tools`).
impl crate::arch_engine::ArchRunner for ArchBackend {
    fn status(&mut self) -> (crate::protocol::KeyState, String, usize) {
        self.warm_model();
        let key = if self.has_key() { crate::protocol::KeyState::Set } else { crate::protocol::KeyState::Missing };
        (key, self.default_model(), self.kb_chunks())
    }
    fn review(
        &mut self,
        path: &str,
        deep: bool,
        sink: &mut dyn FnMut(&str),
        cancel: &crate::engine::CancelFlag,
    ) -> Result<String, String> {
        let root = resolve_root(path);
        let ts = timestamp_now();
        let cancelled = || cancel.is_cancelled();
        let out = ArchBackend::review(self, &root, deep, true, &ts, None, sink, &cancelled)?;
        Ok(out.path.display().to_string())
    }
    fn design(
        &mut self,
        brief: &str,
        deep: bool,
        sink: &mut dyn FnMut(&str),
        cancel: &crate::engine::CancelFlag,
    ) -> Result<String, String> {
        let root = resolve_root("");
        let ts = timestamp_now();
        let cancelled = || cancel.is_cancelled();
        let out = ArchBackend::design(self, &root, brief, deep, &ts, None, sink, &cancelled)?;
        Ok(out.path.display().to_string())
    }
}

/// Resolves a review root: an explicit path, else the current directory.
pub fn resolve_root(path: &str) -> PathBuf {
    if path.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with_chunks() -> Index {
        let mut idx = Index::new();
        idx.add_source("https://cloud.google.com/composer/pricing", "Composer pricing", "Cloud Composer environment pricing is high because it runs always on");
        idx.add_source("https://cloud.google.com/scheduler/docs/overview", "Scheduler", "Cloud Scheduler triggers Cloud Run jobs cheaply for small workloads");
        idx
    }

    #[test]
    fn kb_block_has_numbered_citations_with_urls() {
        let idx = index_with_chunks();
        let block = build_kb_block(&idx, &[0, 1]);
        assert!(block.contains("[1] https://cloud.google.com/composer/pricing"));
        assert!(block.contains("[2] https://cloud.google.com/scheduler/docs/overview"));
        assert!(block.contains("cite as [n]"));
    }

    #[test]
    fn empty_kb_block_is_explicit() {
        let idx = Index::new();
        let block = build_kb_block(&idx, &[]);
        assert!(block.contains("kb update"));
    }

    #[test]
    fn report_prompts_request_every_section() {
        let sys = system_prompt_review("KB");
        for needle in ["Executive summary", "Mermaid", "pillar", "Cost", "CI/CD", "MADR", "Sources"] {
            assert!(sys.contains(needle), "review prompt missing {needle}");
        }
        let d = system_prompt_design("KB", "español");
        assert!(d.contains("español"));
        assert!(d.contains("clarifying questions"));
    }

    #[test]
    fn report_path_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("gentle-arch-rp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = write_report(&dir, "review", "ts", "one").unwrap();
        assert!(p1.ends_with("review-ts.md"));
        let p2 = write_report(&dir, "review", "ts", "two").unwrap();
        assert_ne!(p1, p2, "second write must not overwrite the first");
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retrieval_picks_relevant_chunks() {
        let idx = index_with_chunks();
        let picks = idx.search("composer pricing environment", TOP_K);
        assert_eq!(picks.first().copied(), Some(0));
        let brief_picks = retrieve_for_brief(&idx, "necesito un scheduler para cloud run jobs baratos");
        assert_eq!(brief_picks.first().copied(), Some(1));
    }

    #[test]
    fn brief_language_detects_spanish() {
        assert_eq!(brief_language("Sistema de cálculo de rentabilidad para un banco"), "es");
        assert_eq!(brief_language("A profitability system for a bank"), "en");
    }
}
