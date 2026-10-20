//! `projj vcs` is a whole jj, needing nothing installed.

use std::path::Path;
use std::process::Command;
use std::process::Output;

/// Runs `projj vcs ARGS` in `dir` with no PATH, so no installed jj or git can
/// stand in, and with no user configuration.
fn jj(dir: &Path, home: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_projj"))
        .arg("vcs")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "")
        .env("HOME", home)
        .env("JJ_CONFIG", home.join("config.toml"))
        .env("JJ_USER", "Test")
        .env("JJ_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "projj vcs {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn runs_jj_with_nothing_installed() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "").unwrap();
    let repo = home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let version = jj(&repo, home.path(), &["--version"]);
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.starts_with("jj 0.43.0, built into projj"),
        "{version}"
    );

    jj(&repo, home.path(), &["git", "init"]);
    std::fs::write(repo.join("new.txt"), "hello\n").unwrap();
    let snapshot = jj(&repo, home.path(), &["util", "snapshot"]);
    assert!(String::from_utf8_lossy(&snapshot.stderr).contains("Snapshot complete"));
    let changed = jj(&repo, home.path(), &["diff", "--name-only"]);
    assert_eq!(String::from_utf8_lossy(&changed.stdout), "new.txt\n");
}
