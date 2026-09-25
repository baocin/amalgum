//! Embeds prebuilt remote CLI binaries into the app when `AMALGUM_EMBED_CLI_DIR` is set
//! (release builds, SPEC §5.31 "Remote build", §9). The directory holds
//! `<target-triple>/amalgum` for each remote target; missing ones are simply not embedded.
//! Local and CI builds embed nothing, and the app reports that remote CLI upload is unavailable.

use std::path::PathBuf;
use std::{env, fs};

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
];

fn main() {
    println!("cargo:rerun-if-env-changed=AMALGUM_EMBED_CLI_DIR");
    let mut code = String::from("pub static EMBEDDED_CLI: &[(&str, &[u8])] = &[\n");
    if let Some(dir) = env::var_os("AMALGUM_EMBED_CLI_DIR").map(PathBuf::from) {
        for target in TARGETS {
            let bin = dir.join(target).join("amalgum");
            if bin.is_file() {
                // include_bytes! in the generated file resolves relative to that file's own
                // location ($OUT_DIR), not to the crate root or AMALGUM_EMBED_CLI_DIR — so a
                // relative `dir` must be canonicalized before it is written out.
                let bin = fs::canonicalize(&bin).expect("canonicalize embedded CLI path");
                println!("cargo:rerun-if-changed={}", bin.display());
                code.push_str(&format!(
                    "    ({target:?}, include_bytes!({:?})),\n",
                    bin.display().to_string()
                ));
            }
        }
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("embedded_cli.rs");
    fs::write(out, code).expect("write embedded_cli.rs");
}
