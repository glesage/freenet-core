//! Records the build target and Core revision, so every report can name the
//! exact build it came from.

use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=FREENET_MOBILE_TARGET={target}");

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=FREENET_MOBILE_PROFILE={profile}");

    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .is_some_and(|out| !out.stdout.is_empty());
    let suffix = if dirty { "+dirty" } else { "" };
    println!("cargo:rustc-env=FREENET_MOBILE_CORE_REVISION={revision}{suffix}");
    // Resolved dependency versions, from the workspace lock file.
    let lock = std::fs::read_to_string("../../Cargo.lock").unwrap_or_default();
    for (package, var) in [
        ("freenet-stdlib", "FREENET_MOBILE_STDLIB_VERSION"),
        ("wasmtime", "FREENET_MOBILE_WASMTIME_VERSION"),
        ("uniffi", "FREENET_MOBILE_UNIFFI_VERSION"),
    ] {
        println!(
            "cargo:rustc-env={var}={}",
            highest_locked_version(&lock, package)
        );
    }
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

/// The highest version of `package` in a `Cargo.lock`.
fn highest_locked_version(lock: &str, package: &str) -> String {
    let name_line = format!("name = \"{package}\"");
    let mut best: Option<Vec<u64>> = None;
    let mut best_text = String::from("unknown");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() != name_line {
            continue;
        }
        let Some(version) = lines
            .next()
            .and_then(|l| l.trim().strip_prefix("version = \""))
            .and_then(|l| l.strip_suffix('"'))
        else {
            continue;
        };
        let parts: Vec<u64> = version
            .split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        if best.as_ref().is_none_or(|b| parts > *b) {
            best = Some(parts);
            best_text = version.to_owned();
        }
    }
    best_text
}
