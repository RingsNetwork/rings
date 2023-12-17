use std::process::Command;

use prost_build_config::BuildConfig;
use prost_build_config::Builder;

fn build_json_codec_service() {
    let internal_service = tonic_build::manual::Service::builder()
        .name("InternalService")
        .package("rings_node")
        .method(
            tonic_build::manual::Method::builder()
                .name("connect_peer_via_http")
                .route_name("ConnectPeerViaHttp")
                .input_type("crate::protos::rings_node::ConnectPeerViaHttpRequest")
                .output_type("crate::protos::rings_node::ConnectPeerViaHttpResponse")
                .codec_path("crate::protos::codec::JsonCodec")
                .build(),
        )
        .build();

    tonic_build::manual::Builder::new()
        .out_dir("src/protos")
        .compile(&[internal_service]);

    std::fs::rename(
        "src/protos/rings_node.InternalService.rs",
        "src/protos/rings_node_internal_service.rs",
    )
    .unwrap();
}

fn main() {
    let config_content = include_str!("src/protos/build_config.yaml");
    let config: BuildConfig = serde_yaml::from_str(config_content).unwrap();
    Builder::from(config).build_protos();

    build_json_codec_service();

    Command::new("cargo")
        .args(["+nightly", "fmt"])
        .output()
        .unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/protos/rings.proto");
    println!("cargo:rerun-if-changed=src/protos/build_config.yaml");
}
