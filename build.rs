fn main() {
    tonic_build::configure()
        .compile(&["proto/grpc.proto"], &["proto"])
        .unwrap();
}
