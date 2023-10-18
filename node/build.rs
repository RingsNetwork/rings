extern crate cbindgen;
use std::env;
use std::process::Command;

fn gen_cbinding() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    cbindgen::Builder::new()
        .with_language(cbindgen::Language::C)
        .with_no_includes()
        .with_documentation(true)
        .with_crate(crate_dir)
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file("bindings.h");
}

fn gen_version() {
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        let git_short_hash = String::from_utf8(output.stdout).unwrap();
        println!("cargo:rustc-env=GIT_SHORT_HASH={}", git_short_hash);
    }
}

fn main() {
    gen_cbinding();
    gen_version();
}
