//! Knowledge base for `gentle-arch` (S18): a curated, versioned list of official
//! infrastructure docs (GCP + data first), fetched into a local directory,
//! stripped from HTML to text, chunked, and indexed with an own BM25 index.
//!
//! Everything here is pure except [`fetch_into`], which uses `ureq`. Tests use a
//! fake HTTP server and a per-test KB directory; the real docs are never fetched
//! by a test.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The curated, versioned list of OFFICIAL source URLs, GCP + data first.
///
/// Prefer the plain/markdown variants where they exist. Keep this list between
/// ~20 and ~40 entries; it is the single source of truth for `kb update`.
pub const SOURCES: &[&str] = &[
    // Airflow best practices and GCP operators.
    "https://airflow.apache.org/docs/apache-airflow/stable/best-practices.html",
    "https://airflow.apache.org/docs/apache-airflow-providers-google/stable/operators/cloud/bigquery.html",
    "https://airflow.apache.org/docs/apache-airflow-providers-google/stable/operators/cloud/dataflow.html",
    // Cloud Composer.
    "https://cloud.google.com/composer/docs/concepts/overview",
    "https://cloud.google.com/composer/pricing",
    "https://cloud.google.com/composer/docs/composer-3/optimize-environments",
    // Serverless orchestration alternatives.
    "https://cloud.google.com/scheduler/docs/overview",
    "https://cloud.google.com/run/docs/create-jobs",
    "https://cloud.google.com/workflows/docs/overview",
    // Data warehouse and processing.
    "https://cloud.google.com/bigquery/docs/best-practices-costs",
    "https://cloud.google.com/bigquery/docs/best-practices-performance-overview",
    "https://cloud.google.com/dataflow/docs/overview",
    // Google Cloud Architecture Framework pillars.
    "https://cloud.google.com/architecture/framework/operational-excellence",
    "https://cloud.google.com/architecture/framework/security",
    "https://cloud.google.com/architecture/framework/reliability",
    "https://cloud.google.com/architecture/framework/cost-optimization",
    "https://cloud.google.com/architecture/framework/performance-optimization",
    // GCP blog on orchestrating data workloads.
    "https://cloud.google.com/blog/products/data-analytics/orchestrating-your-data-workloads",
];

/// BM25 parameters (standard defaults).
pub const BM25_K1: f64 = 1.2;
pub const BM25_B: f64 = 0.75;

/// Resolves the KB base directory: `GENTLE_ARCH_KB_DIR` when set (tests), else
/// `<home>/.local/share/gentle-partitioner/kb`.
pub fn kb_dir(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("GENTLE_ARCH_KB_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    home.join(".local").join("share").join("gentle-partitioner").join("kb")
}

/// The index file name inside the KB directory.
pub const INDEX_FILE: &str = "index.json";

// ---------------------------------------------------------------------------
// HTML → text
// ---------------------------------------------------------------------------

/// Strips HTML to readable text: drops `<script>`, `<style>`, `<nav>`, `<head>`
/// and their contents; keeps heading text and code. Collapses runs of blank
/// lines. This is a small, own stripper — not a full HTML parser.
pub fn strip_html(html: &str) -> String {
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    // Tags whose entire content we drop.
    let drop_tags = ["script", "style", "nav", "head"];
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find the tag name.
            let name_start = i + 1 + if lower.as_bytes().get(i + 1) == Some(&b'/') { 1 } else { 0 };
            let mut j = name_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            let tag = lower.get(name_start..j).unwrap_or("");
            if let Some(dt) = drop_tags.iter().find(|t| **t == tag) {
                // Skip to the matching closing tag.
                let close = format!("</{dt}>");
                if let Some(pos) = lower[i..].find(&close) {
                    i += pos + close.len();
                    out.push(' ');
                    continue;
                } else {
                    break;
                }
            }
            // Block-level tags become a newline so headings/paragraphs separate.
            if is_block_tag(tag) {
                out.push('\n');
            }
            // Skip the rest of the tag.
            if let Some(end) = html[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
            continue;
        }
        // Copy a run of text up to the next tag.
        let start = i;
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        out.push_str(&html[start..i]);
    }
    let decoded = decode_entities(&out);
    collapse_whitespace(&decoded)
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "br" | "div" | "li" | "ul" | "ol" | "tr" | "section" | "article" | "header" | "footer"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "code" | "table" | "blockquote"
    )
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Collapses horizontal whitespace runs and limits consecutive blank lines to
/// one, trimming each line.
fn collapse_whitespace(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_run = 0;
    for raw in s.lines() {
        let line: String = {
            let mut words = raw.split_whitespace();
            let mut acc = String::new();
            if let Some(w) = words.next() {
                acc.push_str(w);
                for w in words {
                    acc.push(' ');
                    acc.push_str(w);
                }
            }
            acc
        };
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(line);
        }
    }
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

/// Approximate tokens per chunk and overlap (measured in whitespace tokens, a
/// cheap stand-in for model tokens).
pub const CHUNK_TOKENS: usize = 800;
pub const CHUNK_OVERLAP: usize = 120;

/// Splits `text` into ~[`CHUNK_TOKENS`]-token chunks with [`CHUNK_OVERLAP`]
/// token overlap. Tokens are whitespace-separated; chunk text preserves spacing.
pub fn chunk_text(text: &str) -> Vec<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    if tokens.len() <= CHUNK_TOKENS {
        return vec![tokens.join(" ")];
    }
    let step = CHUNK_TOKENS - CHUNK_OVERLAP;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let end = (start + CHUNK_TOKENS).min(tokens.len());
        chunks.push(tokens[start..end].join(" "));
        if end == tokens.len() {
            break;
        }
        start += step;
    }
    chunks
}

// ---------------------------------------------------------------------------
// Tokenization + BM25
// ---------------------------------------------------------------------------

/// Spanish + English stopwords (small, high-frequency set).
pub const STOPWORDS: &[&str] = &[
    // English
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "is", "are", "be", "with", "as", "by", "at", "from",
    "this", "that", "it", "you", "your", "we", "can", "will", "not", "but", "if", "so", "do", "does",
    // Spanish
    "el", "la", "los", "las", "un", "una", "unos", "unas", "y", "o", "de", "del", "a", "en", "por", "para", "con",
    "que", "se", "su", "sus", "lo", "al", "es", "son", "ser", "como", "más", "mas", "este", "esta", "esto", "no",
];

/// Lowercases and splits on non-alphanumeric Unicode boundaries, dropping
/// stopwords and single characters.
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 1)
        .filter(|t| !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Per-source metadata recorded on a successful fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceMeta {
    pub url: String,
    pub title: String,
    pub fetched_at: String,
    pub sha256: String,
    pub chunks: usize,
}

/// One indexed chunk: its source url and the raw chunk text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub url: String,
    pub title: String,
    pub text: String,
}

/// A serializable BM25 index over the KB chunks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub sources: Vec<SourceMeta>,
    pub chunks: Vec<Chunk>,
    /// A URL that failed its last fetch, with a short reason.
    #[serde(default)]
    pub failures: Vec<FetchFailure>,
    /// Per-chunk term frequencies (parallel to `chunks`).
    tf: Vec<HashMap<String, u32>>,
    /// Per-chunk token length (parallel to `chunks`).
    len: Vec<u32>,
    /// Document frequency per term.
    df: HashMap<String, u32>,
}

/// A source that failed to fetch, kept so `kb status` can list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchFailure {
    pub url: String,
    pub reason: String,
}

impl Index {
    pub fn new() -> Self {
        Index::default()
    }

    /// Number of indexed chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Adds a fetched source: strips, chunks, and indexes it. Returns the chunk
    /// count added.
    pub fn add_source(&mut self, url: &str, title: &str, text: &str) -> usize {
        let chunks = chunk_text(text);
        let added = chunks.len();
        for c in chunks {
            let toks = tokenize(&c);
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            for term in tf.keys() {
                *self.df.entry(term.clone()).or_insert(0) += 1;
            }
            self.len.push(toks.len() as u32);
            self.tf.push(tf);
            self.chunks.push(Chunk { url: url.to_string(), title: title.to_string(), text: c });
        }
        added
    }

    fn avg_len(&self) -> f64 {
        if self.len.is_empty() {
            return 0.0;
        }
        let total: u64 = self.len.iter().map(|&l| l as u64).sum();
        total as f64 / self.len.len() as f64
    }

    /// BM25 score of chunk `idx` for `query_terms`.
    fn score(&self, idx: usize, query_terms: &[String], avg_len: f64) -> f64 {
        let n = self.chunks.len() as f64;
        let dl = self.len[idx] as f64;
        let mut score = 0.0;
        for term in query_terms {
            let f = *self.tf[idx].get(term).unwrap_or(&0) as f64;
            if f == 0.0 {
                continue;
            }
            let df = *self.df.get(term).unwrap_or(&0) as f64;
            // BM25 idf with +1 to stay non-negative.
            let idf = (((n - df + 0.5) / (df + 0.5)) + 1.0).ln();
            let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_len.max(1.0));
            score += idf * (f * (BM25_K1 + 1.0)) / denom;
        }
        score
    }

    /// Returns the indices of the top `k` chunks for `query`, best first. Ties
    /// break by chunk index for determinism.
    pub fn search(&self, query: &str, k: usize) -> Vec<usize> {
        if self.chunks.is_empty() {
            return Vec::new();
        }
        let terms = tokenize(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let avg = self.avg_len();
        let mut scored: Vec<(usize, f64)> =
            (0..self.chunks.len()).map(|i| (i, self.score(i, &terms, avg))).filter(|(_, s)| *s > 0.0).collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        scored.into_iter().take(k).map(|(i, _)| i).collect()
    }

    /// Serializes to pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("index serializes")
    }

    /// Parses from JSON; rebuilds nothing (all fields are serialized).
    pub fn from_json(s: &str) -> Result<Index, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// SHA-256 of bytes as lowercase hex, using an own implementation (no crypto
/// dependency) so the content hash is stable and dependency-free.
pub fn sha256_hex(data: &[u8]) -> String {
    sha256::hex(data)
}

/// Derives a human title from the first heading-like line, else the URL tail.
pub fn derive_title(text: &str, url: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        if t.len() >= 3 && t.len() <= 120 {
            return t.to_string();
        }
    }
    url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url).to_string()
}

/// Outcome of a single source fetch.
pub enum FetchOutcome {
    Ok { title: String, text: String, sha256: String },
    Failed(String),
}

/// Fetches one URL with `ureq`, returning stripped text or a failure reason. A
/// 5 MiB body cap guards memory. `base_override`, when `Some`, replaces the
/// scheme+host so tests can point at a fake server while keeping the path.
pub fn fetch_one(url: &str) -> FetchOutcome {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .build()
        .into();
    match agent.get(url).call() {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            if status != 200 {
                return FetchOutcome::Failed(format!("HTTP {status}"));
            }
            match resp.body_mut().read_to_string() {
                Ok(body) => {
                    let text = strip_html(&body);
                    let sha = sha256_hex(body.as_bytes());
                    let title = derive_title(&text, url);
                    FetchOutcome::Ok { title, text, sha256: sha }
                }
                Err(e) => FetchOutcome::Failed(e.to_string()),
            }
        }
        Err(e) => FetchOutcome::Failed(e.to_string()),
    }
}

/// Fetches every URL in `urls`, building and saving an [`Index`] into `dir`.
/// Failures are recorded and do NOT abort the whole update. Returns the index.
pub fn update(dir: &Path, urls: &[&str], now: &str) -> Result<Index, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut index = Index::new();
    for url in urls {
        match fetch_one(url) {
            FetchOutcome::Ok { title, text, sha256 } => {
                let added = index.add_source(url, &title, &text);
                index.sources.push(SourceMeta {
                    url: url.to_string(),
                    title,
                    fetched_at: now.to_string(),
                    sha256,
                    chunks: added,
                });
            }
            FetchOutcome::Failed(reason) => {
                index.failures.push(FetchFailure { url: url.to_string(), reason });
            }
        }
    }
    let path = dir.join(INDEX_FILE);
    std::fs::write(&path, index.to_json()).map_err(|e| e.to_string())?;
    Ok(index)
}

/// Loads a saved index from `dir`, or an empty one when none exists.
pub fn load(dir: &Path) -> Index {
    let path = dir.join(INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => Index::from_json(&s).unwrap_or_default(),
        Err(_) => Index::default(),
    }
}

// --- tiny SHA-256 (public-domain style, own implementation) -----------------
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub fn hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bitlen = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bitlen.to_be_bytes());
        for block in msg.chunks(64) {
            h = compress(h, block);
        }
        let mut s = String::with_capacity(64);
        for v in h {
            s.push_str(&format!("{v:08x}"));
        }
        s
    }

    fn compress(mut h: [u32; 8], block: &[u8]) -> [u32; 8] {
        let mut w = [0u32; 64];
        for (i, wi) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *wi = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_list_is_curated_and_official() {
        assert!(SOURCES.len() >= 15 && SOURCES.len() <= 40, "about 20-40 URLs, got {}", SOURCES.len());
        for u in SOURCES {
            assert!(u.starts_with("https://"), "official https source: {u}");
        }
        // GCP + data first: Composer, BigQuery, Dataflow, Scheduler, Cloud Run present.
        let joined = SOURCES.join(" ");
        for needle in ["composer", "bigquery", "dataflow", "scheduler", "run", "workflows", "airflow", "architecture"] {
            assert!(joined.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn strip_html_drops_script_style_nav_keeps_headings_and_code() {
        let html = "<html><head><title>T</title></head><nav>MENU</nav><body>\
            <h1>Best Practices</h1><script>var x=secret()</script>\
            <style>.a{color:red}</style><p>Use BigQuery for compute.</p>\
            <pre><code>bq query --nouse_legacy_sql</code></pre></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Best Practices"));
        assert!(text.contains("Use BigQuery for compute."));
        assert!(text.contains("bq query --nouse_legacy_sql"));
        assert!(!text.contains("secret()"), "script content dropped");
        assert!(!text.contains("color:red"), "style content dropped");
        assert!(!text.contains("MENU"), "nav content dropped");
    }

    #[test]
    fn chunking_overlaps_and_covers() {
        let text = (0..2000).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2);
        // First chunk is CHUNK_TOKENS tokens.
        assert_eq!(chunks[0].split_whitespace().count(), CHUNK_TOKENS);
        // Overlap: last OVERLAP tokens of chunk 0 are the first of chunk 1.
        let c0: Vec<&str> = chunks[0].split_whitespace().collect();
        let c1: Vec<&str> = chunks[1].split_whitespace().collect();
        assert_eq!(&c0[CHUNK_TOKENS - CHUNK_OVERLAP..], &c1[..CHUNK_OVERLAP]);
    }

    #[test]
    fn tokenize_lowercases_and_drops_stopwords() {
        let toks = tokenize("The Composer environment IS expensive, y Cloud Run es barato.");
        assert!(toks.contains(&"composer".to_string()));
        assert!(toks.contains(&"environment".to_string()));
        assert!(toks.contains(&"barato".to_string()));
        assert!(!toks.contains(&"the".to_string()));
        assert!(!toks.contains(&"is".to_string()));
        assert!(!toks.contains(&"es".to_string()));
    }

    #[test]
    fn bm25_ranks_the_right_chunk_first() {
        let mut idx = Index::new();
        idx.add_source("u1", "Composer pricing", "Cloud Composer pricing is high because the environment runs always on");
        idx.add_source("u2", "Scheduler", "Cloud Scheduler triggers Cloud Run jobs cheaply for small workloads");
        idx.add_source("u3", "BigQuery", "BigQuery best practices reduce query cost and improve performance");
        let top = idx.search("composer pricing environment", 3);
        assert_eq!(top.first().copied(), Some(0), "the composer chunk ranks first");
        let top2 = idx.search("scheduler cloud run jobs", 3);
        assert_eq!(top2.first().copied(), Some(1));
    }

    #[test]
    fn index_json_round_trips() {
        let mut idx = Index::new();
        idx.add_source("u1", "T1", "Cloud Composer environment pricing details and cost optimization");
        idx.sources.push(SourceMeta {
            url: "u1".into(),
            title: "T1".into(),
            fetched_at: "2026-01-01T00:00:00Z".into(),
            sha256: "abc".into(),
            chunks: 1,
        });
        idx.failures.push(FetchFailure { url: "bad".into(), reason: "HTTP 404".into() });
        let json = idx.to_json();
        let back = Index::from_json(&json).unwrap();
        assert_eq!(back, idx);
        // The restored index still ranks correctly.
        assert_eq!(back.search("composer pricing", 1).first().copied(), Some(0));
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc")
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
