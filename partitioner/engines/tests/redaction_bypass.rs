//! Redaction gaps found by the T18 verification (L47): URL credentials,
//! space-form secret assignments, and GitHub workflow files being skipped.
use gentle_engines::scan::{redact_secrets, scan};

#[test]
fn url_embedded_credentials_are_redacted() {
    let (out, n) = redact_secrets("db = postgres://admin:S3cr3tPass@db.internal:5432/app");
    assert!(!out.contains("S3cr3tPass"), "{out}");
    assert!(out.contains("db.internal"), "the host stays: {out}");
    assert!(n >= 1);
}

#[test]
fn space_form_env_and_arg_secrets_are_redacted() {
    let (out, _) = redact_secrets("ENV DB_PASSWORD Sp4ceValue\nARG API_TOKEN abc123secret\nENV PORT 8080");
    assert!(!out.contains("Sp4ceValue"), "{out}");
    assert!(!out.contains("abc123secret"), "{out}");
    assert!(out.contains("ENV PORT 8080"), "non-secret ENV stays: {out}");
}

#[test]
fn github_workflows_are_scanned_and_redacted_but_dot_git_is_not() {
    let dir = std::env::temp_dir().join(format!("gentle-redact-wf-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"),
        "on: push\njobs:\n  b:\n    env:\n      TOKEN: ghp_abcdefghijklmnopqrstuvwxyz0123456789\n").unwrap();
    std::fs::write(dir.join(".git/config"), "[remote] url = https://u:GitPass@x\n").unwrap();
    let result = scan(&dir);
    let wf = result.files.iter().find(|f| f.path.ends_with("ci.yml")).expect("workflow scanned");
    assert!(!wf.content.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "{}", wf.content);
    assert!(result.files.iter().all(|f| !f.path.starts_with(".git/")), ".git stays excluded");
    std::fs::remove_dir_all(&dir).unwrap();
}
