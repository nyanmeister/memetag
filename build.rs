//! Capture the git revision and build date into env vars for `memetag --version`.
//! Falls back to "unknown" when built outside a git checkout (e.g. from a tarball).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let archive = std::fs::read_to_string("build-revision.txt").unwrap_or_default();
    let archive_fields: Vec<_> = archive.split_whitespace().collect();
    let archive_revision = archive_fields.first().filter(|v| !v.contains("Format"));
    let hash = git(&["rev-parse", "--short", "HEAD"])
        .or_else(|| archive_revision.map(|v| v.to_string()))
        .unwrap_or_else(|| "unknown".into());
    // Mark a build from a dirty tree, so a hand-built binary is never mistaken for a clean commit.
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let rev = if dirty { format!("{hash}+") } else { hash };
    println!("cargo:rustc-env=MEMETAG_GIT={rev}");

    let date = git(&["show", "-s", "--format=%cs", "HEAD"]) // commit date, stable across rebuilds of the same commit
        .or_else(|| archive_revision.and_then(|_| archive_fields.get(1).map(|v| v.to_string())))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MEMETAG_COMMIT_DATE={date}");

    // Re-run when HEAD or the checked-out branch tip moves, so the string stays current without a clean build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=build-revision.txt");
    for path in [
        "src",
        "gui",
        "infer",
        "Cargo.toml",
        "Cargo.lock",
        "packaging",
        "tools",
        "docs",
        "README.md",
        "LICENSE",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.strip_prefix("ref: ").map(str::trim) {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
}
