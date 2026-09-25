//! Regression test for `build.rs`'s `AMALGUM_EMBED_CLI_DIR` embedding (SPEC §5.31, §9).
//!
//! The generated `embedded_cli.rs` lives in `$OUT_DIR` and calls `include_bytes!` with
//! whatever path `build.rs` wrote out; `include_bytes!` resolves a relative path against the
//! file that contains the call (`$OUT_DIR/embedded_cli.rs`), not against the crate root or
//! `AMALGUM_EMBED_CLI_DIR` itself. So a *relative* `AMALGUM_EMBED_CLI_DIR` — the natural thing
//! to type when reproducing a release build locally — must still compile.
//!
//! This builds a throwaway crate that copies the real `build.rs` verbatim (so the test tracks
//! that file, not a reimplementation of it) and points a relative `AMALGUM_EMBED_CLI_DIR` at a
//! stub binary, the same way `scripts/build` would from the repo root.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn relative_embed_cli_dir_compiles() {
    let crate_dir = tempfile::tempdir().expect("crate tempdir");
    let crate_dir = crate_dir.path();
    let target_dir = tempfile::tempdir().expect("nested target tempdir");

    fs::create_dir_all(crate_dir.join("src")).expect("mkdir src");
    fs::create_dir_all(crate_dir.join("embed/x86_64-unknown-linux-musl")).expect("mkdir embed");
    fs::write(crate_dir.join("embed/x86_64-unknown-linux-musl/amalgum"), b"stub-cli-bytes")
        .expect("write stub binary");

    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"embed-cli-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"), crate_dir.join("build.rs"))
        .expect("copy the real build.rs");
    fs::write(
        crate_dir.join("src/main.rs"),
        "mod embedded {\n    include!(concat!(env!(\"OUT_DIR\"), \"/embedded_cli.rs\"));\n}\n\
         fn main() {\n    assert_eq!(embedded::EMBEDDED_CLI.len(), 1);\n}\n",
    )
    .expect("write src/main.rs");

    let output = Command::new("cargo")
        .arg("build")
        .current_dir(crate_dir)
        // Relative, as a human reproducing a release build locally would type it —
        // scripts/build runs from the repo root, and release.yml's absolute
        // `${{ github.workspace }}/embed` is the case that already worked.
        .env("AMALGUM_EMBED_CLI_DIR", "embed")
        .env("CARGO_TARGET_DIR", target_dir.path())
        .output()
        .expect("run cargo build");

    assert!(
        output.status.success(),
        "a relative AMALGUM_EMBED_CLI_DIR must still compile; build.rs must canonicalize the \
         path before writing it into include_bytes!, which resolves relative to $OUT_DIR:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
