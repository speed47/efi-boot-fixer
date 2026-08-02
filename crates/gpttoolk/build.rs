//! Stamps the build with the commit it came from.
//!
//! A continuous build found on someone's ESP months later has to answer one
//! question: which commit is this? `CARGO_PKG_VERSION` cannot, because it only
//! moves when a release is cut. So the version the application reports is
//! computed here from `git describe`, and everything that is not an exact tag
//! carries the commit as semver build metadata: `0.1.0+3.g1a2b3c4`.
//!
//! A build with no git repository at hand - a source tarball, a vendored
//! checkout - falls back to the bare package version rather than failing.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let version = match env_override() {
        Some(v) => v,
        None => match describe() {
            Some(d) => stamp(&pkg, &d),
            None => pkg,
        },
    };
    println!("cargo:rustc-env=GPTTOOLK_VERSION={version}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GPTTOOLK_VERSION");
    // Set by GitHub Actions and different on every commit. A cached target
    // directory would otherwise keep serving a stamp computed for an earlier
    // build, which is worse than no stamp at all: it names the wrong commit.
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// An explicit version, for a build that knows better than git does.
fn env_override() -> Option<String> {
    let v = std::env::var("GPTTOOLK_VERSION").ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn describe() -> Option<String> {
    // --always is what makes this work in a repository with no tags yet: it
    // falls back to the bare commit hash instead of failing.
    let out = git(&["describe", "--tags", "--always", "--dirty", "--match", "v*"])?;
    (!out.is_empty()).then_some(out)
}

/// Turn `git describe` output into the version the application reports.
fn stamp(pkg: &str, describe: &str) -> String {
    let (base, dirty) = match describe.strip_suffix("-dirty") {
        Some(rest) => (rest, true),
        None => (describe, false),
    };

    // Three shapes reach this point: `v0.1.0` sitting on a tag, `v0.1.0-3-g1a2b3c4`
    // some commits past one, and a bare `1a2b3c4` while the repository has no
    // tags at all. Only the first needs no commit in the version.
    let tail: Vec<&str> = base.rsplitn(3, '-').collect();
    let mut suffix = match tail.as_slice() {
        [hash, count, _tag]
            if hash.starts_with('g') && count.chars().all(|c| c.is_ascii_digit()) =>
        {
            format!("{count}.{hash}")
        }
        _ if base.starts_with('v') => {
            if base[1..] != *pkg {
                println!(
                    "cargo:warning=tag {base} does not match package version {pkg}; \
                     the binary will report {pkg}"
                );
            }
            String::new()
        }
        _ => format!("g{base}"),
    };

    if dirty {
        if !suffix.is_empty() {
            suffix.push('.');
        }
        suffix.push_str("dirty");
    }

    if suffix.is_empty() {
        pkg.to_string()
    } else {
        format!("{pkg}+{suffix}")
    }
}

/// The git files whose contents decide the stamp, so that committing or
/// checking out re-runs this script.
fn git_watch_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // A linked worktree keeps HEAD in its own git dir but refs in the common
    // one, so both are asked for. Missing paths are dropped: naming one that
    // does not exist makes cargo re-run this script on every single build.
    let dirs = [git(&["rev-parse", "--absolute-git-dir"]), git(&["rev-parse", "--git-common-dir"])];
    for dir in dirs.into_iter().flatten() {
        let dir = absolute(dir);
        for name in ["HEAD", "refs", "packed-refs"] {
            let path = dir.join(name);
            if path.exists() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// `--git-common-dir` can answer with a path relative to the crate directory,
/// which is where git was run.
fn absolute(dir: String) -> PathBuf {
    let dir = PathBuf::from(dir);
    if dir.is_absolute() {
        return dir;
    }
    match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(manifest) => PathBuf::from(manifest).join(dir),
        Err(_) => dir,
    }
}

/// Run git in the crate directory, or `None` if it is not usable here.
fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git").args(args).current_dir(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
