use std::process::Command;

fn main() {
    prost_build::Config::new()
        .out_dir("src/protos")
        .compile_protos(&["src/protos/rings_node.proto"], &["src/protos"])
        .unwrap();

    Command::new("cargo").args(["fmt"]).output().unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/protos/rings.proto");
}
