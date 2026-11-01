//! Builds the TLS helper DLL (helper/xb1tls.rs) with the same compiler and embeds it via
//! `include_bytes!` in src/tlsdll.rs.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=helper/xb1tls.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let status = Command::new(rustc)
        .args([
            "--edition",
            "2021",
            "-C",
            "opt-level=2",
            "-C",
            "panic=abort",
            "--crate-type",
            "cdylib",
            "--crate-name",
            "xb1tls",
            "helper/xb1tls.rs",
            "--out-dir",
        ])
        .arg(&out)
        .status()
        .expect("failed to run rustc for the TLS helper DLL");
    assert!(status.success(), "building the TLS helper DLL failed");
}
