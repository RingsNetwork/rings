fmt:
	cargo +nightly fmt --all
# require taplo_cli, which can be install with
# cargo install taplo-cli
	taplo format
# require typos_cli, which can be install with
# cargo install typos-cli
	typos --write-changes

clippy-fix:
	cargo clippy --fix --allow-dirty --no-deps

test-core-wasm:
	cd crates/core; wasm-pack test --chrome --features browser_chrome_test --no-default-features

test-node-browser:
	cd crates/node; wasm-pack test --chrome --features browser_chrome_test --no-default-features
