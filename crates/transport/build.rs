fn main() {
    let native_backends = [
        ("dummy", std::env::var_os("CARGO_FEATURE_DUMMY").is_some()),
        (
            "native-webrtc",
            std::env::var_os("CARGO_FEATURE_NATIVE_WEBRTC").is_some(),
        ),
    ]
    .into_iter()
    .filter_map(|(feature, enabled)| enabled.then_some(feature))
    .collect::<Vec<_>>();

    let web_backend = std::env::var_os("CARGO_FEATURE_WEB_SYS_WEBRTC").is_some();

    if web_backend && !native_backends.is_empty() {
        eprintln!(
            "rings-transport feature `web-sys-webrtc` cannot be combined with native transport features: {}.",
            native_backends.join(", ")
        );
        std::process::exit(1);
    }
}
