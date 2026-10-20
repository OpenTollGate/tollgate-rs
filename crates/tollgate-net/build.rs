//! Stamps the binaries with the commit they were built from, so `--version`
//! and the live displays say which build is running.
//!
//! From `TOLLGATE_COMMIT` when the build sets it — a Nix or container build
//! has no `.git` to ask — and otherwise from git.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TOLLGATE_COMMIT");
    let commit = std::env::var("TOLLGATE_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(from_git)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TOLLGATE_COMMIT={commit}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn from_git() -> Option<String> {
    let commit = git(&["rev-parse", "--short=12", "HEAD"])?;
    // Run again when HEAD moves: a checkout changes HEAD, a commit changes the
    // branch it names, and a gc moves that into packed-refs.
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    watch.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for name in watch {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    Some(commit)
}
