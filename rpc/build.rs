use std::process::Command;

fn main() {
    tonic_build::configure()
        .out_dir("src/protos")
        .client_attribute(
            "rings_node.RingsNodeService",
            r#"#[cfg(not(target_arch = "wasm32"))]"#,
        )
        .server_attribute(
            "rings_node.RingsNodeService",
            r#"#[cfg(not(target_arch = "wasm32"))]"#,
        )
        .compile(&["src/protos/rings_node.proto"], &["src/protos"])
        .unwrap();

    Command::new("cargo").args(["fmt"]).output().unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/protos/rings.proto");
}
