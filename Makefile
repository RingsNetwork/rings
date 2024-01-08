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

# You may need to install circomlib by `npm install circomlib` first
build-snark-bitcoin:
# This will generate bitcoin_cpp, thus you may need [https://github.com/nlohmann/json] as deps lib
	cargo run -p crates/snark -- ./examples/snark/bitcoin/circom/bitcoin.circom --r1cs --sym --wasm --prime vesta -o ./examples/snark/bitcoin/circom
