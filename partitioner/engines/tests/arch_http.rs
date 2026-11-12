//! `gentle-arch` review/design against the fake Anthropic server: the prompt
//! carries KB citations and ephemeral cache control, the report is written
//! (never overwriting) and cites its sources, and planted repo secrets never
//! reach the request body.
mod common;

use std::path::Path;

use common::{Fake, Resp};
use gentle_engines::arch::ArchBackend;
use gentle_engines::kb::Index;
use gentle_engines::keystore::MemoryStore;

/// Builds a fake Anthropic that lists a sonnet model and streams a short report.
fn fake_anthropic(report: &'static str) -> Fake {
    Fake::start(move |req, _| {
        if req.path.starts_with("/v1/messages") {
            Resp::sse(format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                serde_json::to_string(report).unwrap()
            ))
        } else {
            Resp::json(
                200,
                r#"{"data":[{"id":"claude-sonnet-4-5","created_at":"2025-09-01T00:00:00Z","line":"sonnet"},
                    {"id":"claude-opus-4-5","created_at":"2025-09-01T00:00:00Z","line":"opus"}],"has_more":false}"#,
            )
        }
    })
}

fn kb_dir_with_chunks() -> Dir {
    let dir = tempdir();
    let mut idx = Index::new();
    idx.add_source(
        "https://cloud.google.com/composer/pricing",
        "Composer pricing",
        "Cloud Composer environment pricing is high because it runs always on; weigh Cloud Scheduler and Cloud Run.",
    );
    idx.add_source(
        "https://cloud.google.com/bigquery/docs/best-practices-costs",
        "BigQuery costs",
        "BigQuery best practices reduce query cost and improve performance by pruning partitions.",
    );
    std::fs::write(dir.path().join(gentle_engines::kb::INDEX_FILE), idx.to_json()).unwrap();
    dir
}

#[test]
fn review_prompt_cites_kb_and_never_leaks_secrets() {
    let fake = fake_anthropic("# Architecture Review\nSee [1].\n");
    let kbd = kb_dir_with_chunks();
    let repo = tempdir();
    // A repo with a DAG (composer) and a planted secret in a tfvars file.
    write(&repo, "dags/etl.py", "from airflow import DAG\n# Cloud Composer + BigQuery\nwith DAG('x'): pass\n");
    write(&repo, "infra/prod.tfvars", "db_password = \"PLAINTEXTSECRET123\"\n");
    write(&repo, ".env", "AWS_SECRET_ACCESS_KEY=AKIAPLANTEDSECRETVALUE1\n");

    let store = MemoryStore::new();
    store_key(&store);
    let mut backend = ArchBackend::new_with_kb(&store, Path::new("/nonexistent-home"), &fake.base, kbd.path());

    let mut out = String::new();
    let result = backend
        .review(repo.path(), false, false, "20260101T000000", None, &mut |t| out.push_str(t), &|| false)
        .unwrap();

    // The report streamed and was written (never overwriting).
    assert!(out.contains("Architecture Review"));
    assert!(result.path.exists());
    assert!(result.path.to_string_lossy().contains(".gentle-arch"));
    let on_disk = std::fs::read_to_string(&result.path).unwrap();
    assert!(on_disk.contains("Architecture Review"));

    // The request carried the KB block with [1] citation + URL and cache_control.
    let reqs = fake.requests();
    let msg = reqs.iter().find(|r| r.path.starts_with("/v1/messages")).expect("a messages request");
    assert!(msg.body.contains("[1] https://cloud.google.com/"), "a numbered citation with a URL");
    assert!(msg.body.contains("https://cloud.google.com/composer/pricing"), "the composer source is cited");
    assert!(msg.body.contains("cite as [n]"), "the KB block instructs citation");
    assert!(msg.body.contains("ephemeral"), "cache_control on the KB/system block");

    // Planted secrets never appear in the request body.
    assert!(!msg.body.contains("PLAINTEXTSECRET123"), "tfvars secret redacted");
    assert!(!msg.body.contains("AKIAPLANTEDSECRETVALUE1"), ".env never read");
    assert!(!msg.body.contains("AWS_SECRET_ACCESS_KEY=AKIA"), ".env never read");
}

#[test]
fn review_does_not_overwrite_an_existing_report() {
    let fake = fake_anthropic("second run\n");
    let kbd = kb_dir_with_chunks();
    let repo = tempdir();
    write(&repo, "main.tf", "resource \"google_composer_environment\" \"x\" {}\n");
    // Pre-existing report with the same timestamp.
    std::fs::create_dir_all(repo.path().join(".gentle-arch")).unwrap();
    std::fs::write(repo.path().join(".gentle-arch/review-20260101T000000.md"), "FIRST").unwrap();

    let store = MemoryStore::new();
    store_key(&store);
    let mut backend = ArchBackend::new_with_kb(&store, Path::new("/nonexistent-home"), &fake.base, kbd.path());
    let result = backend
        .review(repo.path(), false, false, "20260101T000000", None, &mut |_| {}, &|| false)
        .unwrap();
    assert_ne!(result.path.file_name().unwrap(), "review-20260101T000000.md");
    assert_eq!(std::fs::read_to_string(repo.path().join(".gentle-arch/review-20260101T000000.md")).unwrap(), "FIRST");
}

#[test]
fn design_answers_in_the_brief_language_and_writes() {
    let fake = fake_anthropic("# Diseño\nArquitectura de microservicios.\n");
    let kbd = kb_dir_with_chunks();
    let repo = tempdir();
    let store = MemoryStore::new();
    store_key(&store);
    let mut backend = ArchBackend::new_with_kb(&store, Path::new("/nonexistent-home"), &fake.base, kbd.path());
    let mut out = String::new();
    let result = backend
        .design(
            repo.path(),
            "Sistema de cálculo de rentabilidad para un banco con varios procesos",
            false,
            "20260101T000000",
            None,
            &mut |t| out.push_str(t),
            &|| false,
        )
        .unwrap();
    assert!(out.contains("Diseño"));
    assert!(result.path.to_string_lossy().contains("design-"));
    // The system prompt instructed Spanish output.
    let reqs = fake.requests();
    let msg = reqs.iter().find(|r| r.path.starts_with("/v1/messages")).unwrap();
    assert!(msg.body.contains("español"), "design answers in Spanish");
}

// --- helpers ---------------------------------------------------------------

fn store_key(store: &MemoryStore) {
    use gentle_engines::keystore::KeyStore;
    store.save("sk-ant-test-key-123456").unwrap();
}

struct Dir(std::path::PathBuf);
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
    let p = std::env::temp_dir().join(format!("gentle-arch-it-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&p).unwrap();
    Dir(p)
}
fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
fn write(dir: &Dir, rel: &str, content: &str) {
    let p = dir.path().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}
