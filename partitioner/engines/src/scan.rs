//! Repo scan for `gentle-arch review` (S18): selects infrastructure-relevant
//! files, enforces hard exclusions for secret-bearing paths, redacts inline
//! secrets, and caps what is sent to the model. Everything here is pure (a
//! walker plus string transforms); nothing leaves the machine from this module.
//!
//! SECURITY (Risk item 2): repo content is sent to the Anthropic API, so
//! secret-bearing files are never read and inline secrets are redacted before
//! any text is returned. The exclusions and the redaction regexes are both
//! covered by tests.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

/// Hard file-size cap for any single file read (defensive).
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Maximum number of files included in a scan.
pub const MAX_FILES: usize = 200;
/// Maximum total bytes of file content sent to the model (120 KiB).
pub const MAX_TOTAL_BYTES: usize = 120 * 1024;

/// A category of selected file, ordered by review relevance (lower = sent
/// first). DAGs, CI and IaC lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    Dag,
    Ci,
    Iac,
    Container,
    K8s,
    Deps,
    Readme,
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Dag => "airflow-dag",
            Category::Ci => "ci",
            Category::Iac => "iac",
            Category::Container => "container",
            Category::K8s => "k8s",
            Category::Deps => "deps",
            Category::Readme => "readme",
        }
    }
}

/// A file selected for the context, after redaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: String,
    pub category: Category,
    pub content: String,
    /// Count of inline secrets redacted in this file.
    pub redactions: usize,
}

/// An excluded file and the reason (for the manifest and audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub path: String,
    pub reason: String,
}

/// The result of scanning a repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub excluded: Vec<Excluded>,
    /// Files skipped because a cap was hit (path + reason).
    pub capped: Vec<Excluded>,
    pub total_bytes: usize,
}

impl ScanResult {
    /// The set of technology keywords detected, for KB retrieval queries.
    pub fn detected_technologies(&self) -> Vec<String> {
        let mut techs: Vec<String> = Vec::new();
        let push = |t: &str, techs: &mut Vec<String>| {
            if !techs.iter().any(|x| x == t) {
                techs.push(t.to_string());
            }
        };
        for f in &self.files {
            let lower = f.content.to_lowercase();
            match f.category {
                Category::Dag => push("airflow dags orchestration", &mut techs),
                Category::Ci => push("ci cd pipeline deploy", &mut techs),
                Category::Iac => push("terraform infrastructure as code", &mut techs),
                Category::Container => push("docker container", &mut techs),
                Category::K8s => push("kubernetes gke", &mut techs),
                _ => {}
            }
            for (needle, query) in [
                ("composer", "cloud composer pricing optimize"),
                ("bigquery", "bigquery best practices cost performance"),
                ("dataflow", "dataflow streaming batch"),
                ("cloud run", "cloud run jobs"),
                ("scheduler", "cloud scheduler native orchestration"),
                ("workflows", "gcp workflows orchestration"),
                ("airflow", "airflow best practices gcp operators"),
            ] {
                if lower.contains(needle) {
                    push(query, &mut techs);
                }
            }
        }
        techs
    }

    /// A human-readable manifest of what was included and excluded.
    pub fn manifest(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("Included {} files ({} bytes):\n", self.files.len(), self.total_bytes));
        for f in &self.files {
            s.push_str(&format!("  [{}] {} ({} bytes", f.category.label(), f.path, f.content.len()));
            if f.redactions > 0 {
                s.push_str(&format!(", {} redacted", f.redactions));
            }
            s.push_str(")\n");
        }
        if !self.capped.is_empty() {
            s.push_str(&format!("Skipped {} files (caps):\n", self.capped.len()));
            for e in &self.capped {
                s.push_str(&format!("  {} — {}\n", e.path, e.reason));
            }
        }
        if !self.excluded.is_empty() {
            s.push_str(&format!("Excluded {} paths (secrets/binaries):\n", self.excluded.len()));
            for e in &self.excluded {
                s.push_str(&format!("  {} — {}\n", e.path, e.reason));
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Exclusions (secret-bearing or noise)
// ---------------------------------------------------------------------------

/// Directory names never descended into.
pub const EXCLUDED_DIRS: &[&str] = &[".git", "node_modules", "target", "venv", ".venv", "__pycache__", ".terraform"];

/// Returns a reason when `name` (a file name) must never be read/sent, else
/// `None`. This is the secret guard; it runs before any content is read.
pub fn exclusion_reason(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let rules: [(&dyn Fn(&str) -> bool, &str); 8] = [
        (&|n: &str| n.starts_with(".env"), "env file"),
        (&|n: &str| n.ends_with(".pem"), "PEM file"),
        (&|n: &str| n.ends_with(".key"), "private key file"),
        (&|n: &str| n.starts_with("id_rsa"), "SSH private key"),
        (&|n: &str| n.contains("credentials"), "credentials file"),
        (&|n: &str| n.contains(".tfstate"), "terraform state"),
        (&|n: &str| n.ends_with(".p12") || n.ends_with(".pfx"), "keystore"),
        (&|n: &str| n.ends_with(".crt") || n.ends_with(".cer"), "certificate"),
    ];
    for (pred, reason) in rules {
        if pred(&lower) {
            return Some(reason.to_string());
        }
    }
    None
}

/// Returns a reason when JSON `content` looks like a service-account key
/// (contains a `private_key`), else `None`.
pub fn service_account_reason(content: &str) -> Option<String> {
    if content.contains("\"private_key\"") || content.contains("private_key_id") {
        Some("service-account JSON (private_key)".to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

struct Redactors {
    patterns: Vec<Regex>,
}

fn redactors() -> &'static Redactors {
    static R: OnceLock<Redactors> = OnceLock::new();
    R.get_or_init(|| {
        let patterns = [
            // AWS access key id.
            r"AKIA[0-9A-Z]{16}",
            // AWS secret access key assignment.
            r"(?i)aws_secret_access_key\s*[:=]\s*\S+",
            // GCP API key.
            r"AIza[0-9A-Za-z\-_]{35}",
            // Slack tokens.
            r"xox[baprs]-[0-9A-Za-z-]{10,}",
            // GitHub tokens.
            r"gh[pousr]_[0-9A-Za-z]{20,}",
            r"github_pat_[0-9A-Za-z_]{20,}",
            // Anthropic keys.
            r"sk-ant-[0-9A-Za-z\-_]{10,}",
            // OpenAI-style keys.
            r"sk-[0-9A-Za-z]{20,}",
            // Generic secret assignments.
            r"(?i)(password|secret|token|api[_-]?key)\s*[:=]\s*\S+",
            // Credentials embedded in URLs (scheme://user:pass@host): the userinfo goes.
            r"(?i)\b[a-z][a-z0-9+.\-]*://[^\s/:@]+:[^\s/@]+@",
            // Space-form secret assignments (Dockerfile ENV/ARG, shell-like configs).
            r"(?im)^(\s*(?:ENV|ARG)\s+)?[A-Z0-9_]*(?:PASSWORD|PASSWD|SECRET|TOKEN|API_?KEY|PRIVATE_?KEY)[A-Z0-9_]*\s+\S+",
        ];
        Redactors { patterns: patterns.iter().map(|p| Regex::new(p).expect("valid redaction regex")).collect() }
    })
}

/// Redacts inline secrets in `text`, returning the redacted text and the number
/// of substitutions. PEM private-key blocks are collapsed to a marker.
pub fn redact_secrets(text: &str) -> (String, usize) {
    let mut count = 0usize;
    // PEM blocks first (multi-line).
    let pem = pem_regex();
    let mut out = pem
        .replace_all(text, |_: &regex::Captures| {
            count += 1;
            "<redacted:PEM>".to_string()
        })
        .into_owned();
    for re in &redactors().patterns {
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                count += 1;
                // Preserve the key name for assignment-style matches when there
                // is a capture group; otherwise emit a bare marker.
                if let Some(k) = caps.get(1) {
                    format!("{}=<redacted>", k.as_str())
                } else {
                    "<redacted>".to_string()
                }
            })
            .into_owned();
    }
    (out, count)
}

fn pem_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----")
            .expect("valid PEM regex")
    })
}

/// Redacts the VALUES in a `*.tfvars` file, keeping keys: `key = "x"` →
/// `key = <redacted>`. Comments and blank lines are preserved.
pub fn redact_tfvars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(eq) = line.find('=') {
            let (key, _) = line.split_at(eq);
            out.push_str(key);
            out.push_str("= <redacted>\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Classifies a candidate file by its repo-relative path and content. Returns
/// `None` when the file is not infrastructure-relevant. DAGs require a `DAG(` or
/// `@dag` marker in the content.
pub fn classify(rel_path: &str, content: &str) -> Option<Category> {
    let p = rel_path.replace('\\', "/");
    let lower = p.to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p).to_lowercase();

    if lower.ends_with(".py") && (content.contains("DAG(") || content.contains("@dag")) {
        return Some(Category::Dag);
    }
    if lower.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml")) {
        return Some(Category::Ci);
    }
    if name.starts_with("cloudbuild") && (name.ends_with(".yaml") || name.ends_with(".yml")) {
        return Some(Category::Ci);
    }
    if name == ".gitlab-ci.yml" {
        return Some(Category::Ci);
    }
    if lower.ends_with(".tf") || lower.ends_with(".tfvars") {
        return Some(Category::Iac);
    }
    if name.starts_with("dockerfile") || name.starts_with("docker-compose") {
        return Some(Category::Container);
    }
    if matches!(name.as_str(), "requirements.txt" | "pyproject.toml" | "package.json") {
        return Some(Category::Deps);
    }
    if name == "readme.md" || name == "readme" {
        return Some(Category::Readme);
    }
    // k8s manifests: yaml with apiVersion + kind.
    if (lower.ends_with(".yml") || lower.ends_with(".yaml"))
        && content.contains("apiVersion")
        && content.contains("kind:")
    {
        return Some(Category::K8s);
    }
    None
}

/// Prepares the content to send for a file: tfvars values redacted, then inline
/// secrets redacted everywhere. Returns the content and the redaction count.
pub fn prepare_content(rel_path: &str, raw: &str, category: Category) -> (String, usize) {
    let base = if category == Category::Iac && rel_path.to_lowercase().ends_with(".tfvars") {
        redact_tfvars(raw)
    } else {
        raw.to_string()
    };
    redact_secrets(&base)
}

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

/// Recursively collects candidate file paths under `root`, skipping excluded
/// directories. Returns `(rel_path, abs_path)` pairs, sorted for determinism.
fn walk(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_dir() {
                // Skip only the .git directory itself; .github holds CI workflows to review.
                if EXCLUDED_DIRS.contains(&name.as_str()) || name == ".git" {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push((rel.to_string_lossy().replace('\\', "/"), path));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Scans `root`, selecting infrastructure files with bounded reads, enforcing
/// exclusions and caps, and redacting secrets. The result is ordered by
/// category relevance (DAGs, CI, IaC first).
pub fn scan(root: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let mut selected: Vec<ScannedFile> = Vec::new();

    for (rel, abs) in walk(root) {
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        // Hard secret exclusion by name — never even read the file.
        if let Some(reason) = exclusion_reason(name) {
            result.excluded.push(Excluded { path: rel.clone(), reason });
            continue;
        }
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() as usize > MAX_FILE_BYTES {
            // Only report oversize for files we would otherwise select.
            continue;
        }
        let raw = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(_) => continue, // binary / non-utf8: skip silently
        };
        // Service-account JSON by content.
        if name.ends_with(".json") {
            if let Some(reason) = service_account_reason(&raw) {
                result.excluded.push(Excluded { path: rel.clone(), reason });
                continue;
            }
        }
        let Some(category) = classify(&rel, &raw) else { continue };
        let (content, redactions) = prepare_content(&rel, &raw, category);
        selected.push(ScannedFile { path: rel, category, content, redactions });
    }

    // Order by category relevance, then path, and enforce the caps.
    selected.sort_by(|a, b| a.category.cmp(&b.category).then(a.path.cmp(&b.path)));
    for f in selected {
        if result.files.len() >= MAX_FILES {
            result.capped.push(Excluded { path: f.path, reason: format!("over {MAX_FILES} file cap") });
            continue;
        }
        if result.total_bytes + f.content.len() > MAX_TOTAL_BYTES {
            result.capped.push(Excluded { path: f.path, reason: format!("over {MAX_TOTAL_BYTES} byte cap") });
            continue;
        }
        result.total_bytes += f.content.len();
        result.files.push(f);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_secret_bearing_names() {
        for (name, needle) in [
            (".env", "env"),
            (".env.local", "env"),
            ("server.pem", "PEM"),
            ("tls.key", "private key"),
            ("id_rsa", "SSH"),
            ("gcp-credentials.json", "credentials"),
            ("terraform.tfstate", "state"),
            ("backup.tfstate.backup", "state"),
        ] {
            let r = exclusion_reason(name);
            assert!(r.is_some(), "{name} should be excluded");
            assert!(r.unwrap().to_lowercase().contains(&needle.to_lowercase()), "{name}");
        }
        // A normal file is not excluded.
        assert_eq!(exclusion_reason("main.tf"), None);
        assert_eq!(exclusion_reason("dag.py"), None);
    }

    #[test]
    fn service_account_json_is_detected() {
        let sa = r#"{"type":"service_account","private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n","client_email":"x@y.iam"}"#;
        assert!(service_account_reason(sa).is_some());
        assert_eq!(service_account_reason(r#"{"name":"ok"}"#), None);
    }

    #[test]
    fn redaction_catches_every_token_type() {
        let cases = [
            "AKIAIOSFODNN7EXAMPLE",
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "AIzaSyB1234567890abcdefghijklmnopqrstuvw",
            concat!("xoxb-", "123456789012-1234567890123-aBcDeFgHiJkLmNoPqRsTuVwX"),
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "password: hunter2supersecret",
            "api_key=AKIAVERYSECRETVALUE1",
        ];
        for c in cases {
            let (red, n) = redact_secrets(c);
            assert!(n >= 1, "no redaction for: {c}");
            assert!(red.contains("<redacted>"), "no marker for: {c}");
        }
        // The planted literal secret value must not survive.
        let (red, _) = redact_secrets("ghp_0123456789abcdefghijklmnopqrstuvwxyz");
        assert!(!red.contains("ghp_0123456789abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redaction_handles_pem_blocks() {
        let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIabc\nDEFghi\n-----END RSA PRIVATE KEY-----\nafter";
        let (red, n) = redact_secrets(text);
        assert_eq!(n, 1);
        assert!(red.contains("<redacted:PEM>"));
        assert!(red.contains("before") && red.contains("after"));
        assert!(!red.contains("MIIabc"));
    }

    #[test]
    fn tfvars_values_are_redacted_keys_kept() {
        let tf = "# comment\nproject_id = \"acme-prod\"\ndb_password = \"s3cr3t\"\nregion = \"us-central1\"\n";
        let red = redact_tfvars(tf);
        assert!(red.contains("# comment"));
        assert!(red.contains("project_id = <redacted>"));
        assert!(red.contains("db_password = <redacted>"));
        assert!(!red.contains("acme-prod"));
        assert!(!red.contains("s3cr3t"));
    }

    #[test]
    fn classify_recognises_the_categories() {
        assert_eq!(classify("dags/etl.py", "from airflow import DAG\nwith DAG('x') as d: pass"), Some(Category::Dag));
        assert_eq!(classify("util.py", "print('no dag here')"), None);
        assert_eq!(classify(".github/workflows/ci.yml", "on: push"), Some(Category::Ci));
        assert_eq!(classify("cloudbuild.yaml", "steps: []"), Some(Category::Ci));
        assert_eq!(classify("infra/main.tf", "resource {}"), Some(Category::Iac));
        assert_eq!(classify("prod.tfvars", "x=1"), Some(Category::Iac));
        assert_eq!(classify("Dockerfile", "FROM python"), Some(Category::Container));
        assert_eq!(classify("requirements.txt", "flask"), Some(Category::Deps));
        assert_eq!(classify("README.md", "# hi"), Some(Category::Readme));
        assert_eq!(classify("k8s/deploy.yaml", "apiVersion: apps/v1\nkind: Deployment"), Some(Category::K8s));
    }

    #[test]
    fn scan_orders_by_relevance_and_applies_manifest() {
        let dir = tempdir();
        write(&dir, "dags/etl.py", "from airflow import DAG\nwith DAG('x'): pass\n");
        write(&dir, ".github/workflows/ci.yml", "on: push\njobs: {}\n");
        write(&dir, "infra/main.tf", "resource \"google_composer_environment\" \"x\" {}\n");
        write(&dir, "README.md", "# Project\nUses BigQuery and Dataflow.\n");
        write(&dir, "Dockerfile", "FROM python:3.11\n");
        // Secrets that must be excluded / never sent.
        write(&dir, ".env", "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMIsupersecretvalue123\n");
        write(&dir, "sa.json", r#"{"private_key":"-----BEGIN PRIVATE KEY-----\nABC\n-----END PRIVATE KEY-----"}"#);
        write(&dir, "prod.tfvars", "db_password = \"s3cr3tvalue\"\n");

        let r = scan(dir.path());
        let cats: Vec<Category> = r.files.iter().map(|f| f.category).collect();
        // DAG, then CI, then IaC come before Container/Readme.
        assert_eq!(cats.first(), Some(&Category::Dag));
        assert!(cats.windows(2).all(|w| w[0] <= w[1]), "sorted by relevance: {cats:?}");

        // The .env and the service-account JSON are excluded by name/content.
        assert!(r.excluded.iter().any(|e| e.path == ".env"));
        assert!(r.excluded.iter().any(|e| e.path == "sa.json"));

        // The planted secrets never appear in any sent content.
        let all: String = r.files.iter().map(|f| f.content.clone()).collect();
        assert!(!all.contains("supersecretvalue123"));
        assert!(!all.contains("s3cr3tvalue"));
        assert!(!all.contains("BEGIN PRIVATE KEY"));

        // The tfvars file is included but its value is redacted.
        let tfvars = r.files.iter().find(|f| f.path == "prod.tfvars").expect("tfvars included");
        assert!(tfvars.content.contains("db_password"), "the key is kept");
        assert!(tfvars.content.contains("<redacted>") && !tfvars.content.contains("s3cr3tvalue"), "the value is gone");

        let manifest = r.manifest();
        assert!(manifest.contains("dags/etl.py"));
        assert!(manifest.contains(".env"));
    }

    #[test]
    fn detected_technologies_drive_queries() {
        let dir = tempdir();
        write(&dir, "dags/etl.py", "from airflow import DAG\n# uses Cloud Composer and BigQuery\nwith DAG('x'): pass\n");
        let r = scan(dir.path());
        let techs = r.detected_technologies();
        assert!(techs.iter().any(|t| t.contains("airflow")));
        assert!(techs.iter().any(|t| t.contains("composer")));
        assert!(techs.iter().any(|t| t.contains("bigquery")));
    }

    // --- tiny temp-dir helper, confined to this test's own directory ---------
    struct Dir(PathBuf);
    impl Dir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> Dir {
        let base = std::env::temp_dir();
        let unique = format!("gentle-arch-scan-{}-{:?}", std::process::id(), std::time::SystemTime::now());
        let p = base.join(unique.replace([' ', ':', '.'], "_"));
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn write(dir: &Dir, rel: &str, content: &str) {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}
