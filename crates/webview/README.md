# rings-webview

Reusable, UI-independent webview gateway primitives for serving remote pages through a Rings-owned onion gateway route.

## Capabilities

- **Target & route policy (`url`, `route`)**: `TargetUrl`, `GatewayPrefix`, `GatewayRoute`, and fail-closed `GatewayRoutePolicy`, using `rings-network-policy` to reject private, loopback, link-local, and local-domain targets before transport execution.
- **Pluggable transport (`transport`, `types`)**: `GatewayTransport`, `ConcurrentWebviewGateway`, `GatewayResponseBodyLimit`, typed `GatewayRequest` / `GatewayResponse` DTOs, and trusted `source_target` origin derivation.
- **Isolation & normalization (`header`, `rewrite`, `cookie`, `cors`)**: `HeaderPolicy`, streaming `lol_html` HTML/CSS rewriting (`RewriteContext`), virtual target-origin `CookieJar`, and virtual CORS preflight/response validation (`preflight_request`, `validate_response`).
- **Opaque-origin security boundary**: production navigation responses enforce CSP `sandbox` without `allow-same-origin` so pages run in an isolated opaque origin and cannot access controlled same-origin runtime routes via `fetch` or `XMLHttpRequest`.
- **Optional `browser` feature**: enables browser bootstrap helpers (`browser`) backed by `js-sys` for hosts providing an explicit runtime-request bridge.
