<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings-white.svg">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings.svg" width="128" height="128">
</picture>

Rings Derive
===============

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-derive.svg)](https://crates.io/crates/rings-derive)
[![docs](https://docs.rs/rings-derive/badge.svg?version=1.0.0-pre.11)](https://docs.rs/rings-derive/1.0.0-pre.11/rings_derive/)

Procedural macro crate shared by Rings workspace crates.

## Macros

- `#[wasm_export]`: attribute macro that conditionally attaches `#[wasm_bindgen::prelude::wasm_bindgen]` when compiling for `target_family = "wasm"` (for structs that must be exported as JS classes, use `wasm_bindgen` directly). It rejects attribute arguments at compile time.

## Features

- `wasm`: declares WASM export compatibility across Rings build configurations.
