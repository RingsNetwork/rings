#[cfg(feature = "ffi")]
extern crate cbindgen;
use std::process::Command;

fn gen_version() {
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        let git_short_hash = String::from_utf8(output.stdout).unwrap();
        println!("cargo:rustc-env=GIT_SHORT_HASH={git_short_hash}");
    }
}

fn main() {
    gen_version();
}
