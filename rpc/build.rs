use std::process::Command;

use prost_build_config::BuildConfig;
use prost_build_config::Builder;

fn main() {
    let config_content = include_str!("src/protos/build_config.yaml");
    let config: BuildConfig = serde_yaml::from_str(config_content).unwrap();
    Builder::from(config).build_protos();

    Command::new("cargo")
        .args(["+nightly", "fmt"])
        .output()
        .unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/protos/rings_node.proto");
    println!("cargo:rerun-if-changed=src/protos/build_config.yaml");
}
