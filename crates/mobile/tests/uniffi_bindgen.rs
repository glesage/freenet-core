//! Smoke test for the UniFFI binding generator against this crate's own
//! compiled library.
//!
//! The generated Swift surface, and the `#[uniffi::export(with_foreign)]`
//! dispatch it drives, are never otherwise exercised by any test in this
//! suite (TEST-PLAN-mobile-phase1.md M4.6) — every test elsewhere implements
//! `ContractUpdateListener` as a plain Rust trait, never through generated
//! bindings, and nothing else ever runs the `uniffi-bindgen` binary. This
//! test only smoke-tests CODE GENERATION: that the generator runs against
//! the compiled library and emits a Swift file naming this crate's exported
//! types. It does not exercise dispatch through the generated code — that
//! would need an actual Swift toolchain (see M6 in the same plan, a separate
//! repo).

use std::path::PathBuf;
use std::process::Command;

fn workspace_target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let manifest_dir = env!("CARGO_MANIFEST_DIR");
            std::path::Path::new(manifest_dir)
                .ancestors()
                .find(|p| p.join("Cargo.lock").is_file())
                .expect("workspace root (Cargo.lock) not found above this crate")
                .join("target")
        })
}

#[test]
fn generated_swift_names_the_exported_types() {
    let target = workspace_target_dir();

    // Build the cdylib the generator reads its metadata from. A plain
    // `cargo test` only guarantees the rlib the test binary itself links
    // against, not the cdylib crate-type — build it explicitly.
    let build = Command::new(env!("CARGO"))
        .args(["build", "-p", "freenet-mobile", "--lib"])
        .status()
        .expect("run cargo build");
    assert!(
        build.success(),
        "cargo build -p freenet-mobile --lib failed"
    );

    let dylib_name = if cfg!(target_os = "macos") {
        "libfreenet_mobile.dylib"
    } else if cfg!(target_os = "windows") {
        "freenet_mobile.dll"
    } else {
        "libfreenet_mobile.so"
    };
    let dylib = target.join("debug").join(dylib_name);
    assert!(
        dylib.is_file(),
        "expected the just-built library at {}",
        dylib.display()
    );

    let out_dir = tempfile::tempdir().expect("tempdir");
    let generate = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "freenet-mobile",
            "--bin",
            "uniffi-bindgen",
            "--",
            "generate",
            "--library",
        ])
        .arg(&dylib)
        .args(["--language", "swift", "--out-dir"])
        .arg(out_dir.path())
        .status()
        .expect("run uniffi-bindgen");
    assert!(generate.success(), "uniffi-bindgen generate failed");

    let swift = std::fs::read_to_string(out_dir.path().join("FreenetMobile.swift"))
        .expect("generated FreenetMobile.swift must exist");
    for name in [
        "FreenetNode",
        "MobileProfile",
        "ContractUpdateListener",
        "NodeStatus",
        "GetResult",
    ] {
        assert!(
            swift.contains(name),
            "generated Swift must name the exported type {name}"
        );
    }
}
