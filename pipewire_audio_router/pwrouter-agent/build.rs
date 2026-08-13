//! Bakes a **build identity** into the binary, because `0.1.0` cannot tell two builds
//! apart and this binary is copied around by hand.
//!
//! The agent runs on someone else's machine, so unlike the daemon (which reads
//! `ADDON_VERSION` from its container env at runtime) it has to carry its identity
//! *inside* the executable. Three sources, in order:
//!
//! 1. **`PWROUTER_BUILD`** set at build time — what the `Dockerfile`'s agent stage
//!    passes, so a binary downloaded from the add-on says which add-on build it came
//!    from (`0.3.20260812230112`). That is the number a user can compare with the
//!    add-on's own version.
//! 2. **`git describe --always --dirty`** plus a UTC minute, for builds from a checkout.
//!    The `-dirty` suffix and the timestamp are the point: an uncommitted tree rebuilt
//!    three times produces three distinguishable binaries, which is exactly the case
//!    that made this necessary.
//! 3. **`dev`** plus the timestamp, when there is no git either.
//!
//! Exposed as `env!("PWROUTER_BUILD")`, which is therefore always set.

use std::process::Command;

fn main() {
    // An explicit build id wins and is used verbatim: the add-on's own version string
    // is more useful to a user than anything derived here.
    println!("cargo:rerun-if-env-changed=PWROUTER_BUILD");
    if let Ok(explicit) = std::env::var("PWROUTER_BUILD") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            println!("cargo:rustc-env=PWROUTER_BUILD={explicit}");
            return;
        }
    }

    let revision =
        git(&["describe", "--always", "--dirty", "--tags"]).unwrap_or_else(|| "dev".to_string());
    // **Every `rerun-if` line narrows cargo's default**, which is "re-run when any file
    // in the package changes". Emitting only the git paths below would therefore freeze
    // the stamp for source-only edits — two different binaries claiming one identity,
    // the exact trap this file exists to close — so the package's own inputs are named
    // back. A directory is walked recursively by cargo, so `src` covers the crate.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=pwrouter-agent.service");
    // And when HEAD moves, so committing refreshes the revision without touching a file.
    // The *dirty* flag cannot be tracked this way (a working-tree edit does not touch
    // these), which is the other reason the timestamp carries weight for local builds.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }

    // `date` rather than a crate: this is a build script, the tool is everywhere we
    // build, and pulling `chrono`/`jiff` into the agent for one string would be silly.
    let stamp = Command::new("date")
        .args(["-u", "+%Y%m%dT%H%MZ"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let build = if stamp.is_empty() {
        revision
    } else {
        format!("{revision} {stamp}")
    };
    println!("cargo:rustc-env=PWROUTER_BUILD={build}");
}

/// One `git` invocation in the crate's directory, or `None` for anything that is not a
/// clean success (no git, no repository, a shallow export).
fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}
