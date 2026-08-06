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
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let version = match env_override() {
        Some(v) => v,
        None => match describe() {
            Some(d) => stamp(&pkg, &d),
            None => pkg,
        },
    };
    println!("cargo:rustc-env=BOOTFIXR_VERSION={version}");
    println!("cargo:rustc-env=BOOTFIXR_COMPILE_DATE={}", compile_date());

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BOOTFIXR_VERSION");
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
    let v = std::env::var("BOOTFIXR_VERSION").ok()?;
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

/// The build timestamp, in RFC 3339 format and forced to UTC, so the binary
/// can report it regardless of the timezone the build ran in. Computed here
/// rather than at runtime because the target is `no_std`: no calendar
/// library is available on that side to turn a clock reading into a date.
fn compile_date() -> String {
    let secs =
        SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before epoch").as_secs()
            as i64;
    let days = secs.div_euclid(86400);
    let time_of_day = secs.rem_euclid(86400);
    let (hour, minute, second) = (time_of_day / 3600, (time_of_day / 60) % 60, time_of_day % 60);
    let (year, month, day) = civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic Gregorian (year, month, day), without pulling in a calendar
/// crate just for this.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
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
