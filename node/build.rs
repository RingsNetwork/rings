#[cfg(feature = "ffi")]
extern crate cbindgen;
use std::process::Command;

#[cfg(feature = "ffi")]
fn gen_cbinding() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    match cbindgen::Builder::new()
        .with_language(cbindgen::Language::C)
        .with_no_includes()
        .with_documentation(true)
        .with_crate(crate_dir)
        .generate()
    {
        Ok(g) => {
            g.write_to_file("bindings.h");
        }
        Err(e) => println!("Unable to generate bindings, {:?}", e),
    };
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
    #[cfg(feature = "ffi")]
    gen_cbinding();
    gen_version();
}
