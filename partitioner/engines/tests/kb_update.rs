//! `kb update` against a fake docs server: partial failures do not abort the
//! whole update, metadata is recorded, and the index is saved and reloadable.
//! The real docs are never fetched here.
mod common;

use common::{Fake, Resp};
use gentle_engines::kb;

#[test]
fn kb_update_handles_partial_failures_and_saves() {
    let fake = Fake::start(|req, _| {
        if req.path.contains("/ok1") {
            Resp {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<html><head><title>Composer</title></head><body><h1>Cloud Composer</h1>\
                       <p>Composer environment pricing is high; consider Cloud Scheduler and Cloud Run.</p>\
                       <script>secret()</script></body></html>"
                    .into(),
            }
        } else if req.path.contains("/ok2") {
            Resp {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: "<html><body><h1>BigQuery</h1><p>BigQuery best practices reduce cost and improve performance.</p></body></html>".into(),
            }
        } else {
            // The /bad path fails.
            Resp::json(404, "not found")
        }
    });

    let dir = tempdir();
    let urls = [
        format!("{}/ok1", fake.base),
        format!("{}/bad", fake.base),
        format!("{}/ok2", fake.base),
    ];
    let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    let index = kb::update(dir.path(), &refs, "2026-01-02T03:04:05Z").unwrap();

    // Two sources succeeded, one failed; the failure did not abort the update.
    assert_eq!(index.sources.len(), 2, "two sources fetched");
    assert_eq!(index.failures.len(), 1, "one failure recorded");
    assert!(index.failures[0].url.ends_with("/bad"));
    assert!(index.chunk_count() >= 2);

    // The script content was stripped before indexing.
    assert!(!index.chunks.iter().any(|c| c.text.contains("secret()")));

    // Metadata recorded: url, fetched_at, sha256.
    let s = &index.sources[0];
    assert!(s.url.ends_with("/ok1"));
    assert_eq!(s.fetched_at, "2026-01-02T03:04:05Z");
    assert_eq!(s.sha256.len(), 64);

    // The saved index reloads and still ranks the composer chunk for a composer query.
    let reloaded = kb::load(dir.path());
    assert_eq!(reloaded.sources.len(), 2);
    let top = reloaded.search("composer pricing environment", 3);
    assert!(!top.is_empty());
    let first = &reloaded.chunks[top[0]];
    assert!(first.url.ends_with("/ok1"), "composer query ranks the composer source first");
}

// A temp dir confined to this test, removed on drop.
struct Dir(std::path::PathBuf);
impl Dir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tempdir() -> Dir {
    let p = std::env::temp_dir().join(format!("gentle-arch-kb-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&p).unwrap();
    Dir(p)
}
fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
