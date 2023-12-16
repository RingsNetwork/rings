use std::process::Command;

fn main() {
    prost_serde::build_with_serde(include_str!("src/protos/build_opts.json"));

    Command::new("cargo").args(["fmt"]).output().unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/protos/rings.proto");
}
