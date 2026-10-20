<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings-white.svg">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings.svg" width="128" height="128">
</picture>


Rings RPC
===============

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)


The former HTTP service-backend DTOs (`rings_rpc::types::{HttpRequest, Timeout}`)
are removed. The extension registry and relay protocol replaced that backend in
#596; these DTOs are not used by the JSON-RPC client or current request types in
`protos`. This is a Rust API removal, not a change to the current JSON-RPC wire format.

## Authenticated Rust clients

`jsonrpc::Client::new` and `with_bearer_token` return `Result`. Bearer credentials
require HTTPS, except HTTP to a parsed IPv4 loopback address (`127.0.0.0/8`) or
IPv6 loopback (`::1`). HTTP hostnames, including `localhost`, are rejected; use
`http://127.0.0.1:50000` or `http://[::1]:50000` for a local node. URL userinfo and
fragments are also rejected for authenticated endpoints.

Native RPC clients disable redirects and proxies. This prevents redirect downgrade
and keeps the loopback HTTP exception from sending credentials through a proxy.
Custom native DNS pins, TLS roots and timeouts are retained through
`with_http_client_builder`; it replaces `with_http_client`, since an already-built
transport cannot have its redirect policy secured. Normal TLS certificate
validation remains enabled by default. Authenticated browser RPC uses Fetch with
`redirect: "error"`; its transport settings are controlled by the browser rather
than the supplied reqwest builder. Unauthenticated HTTP endpoints remain supported.

## JSON-RPC methods

The method reference, with every method's params, result, listener and authorization, is the
[API page of the Rings book](https://rings.rs/docs/jsonrpc.html)
([source](../../docs/src/jsonrpc.md)).
