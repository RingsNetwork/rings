fmt:
	cargo +nightly fmt -p rings-core
	cargo +nightly fmt -p rings-node
	taplo format

clippy-fix:
	cargo clippy --fix --allow-dirty

build-browser-pack:
	wasm-pack build node --scope ringsnetwork -t web --no-default-features --features browser --features console_error_panic_hook
test-core-wasm:
	cd core; wasm-pack test --chrome --features browser_chrome_test --no-default-features

test-node-browser:
	cd node; wasm-pack test --chrome --features browser_chrome_test --no-default-features
