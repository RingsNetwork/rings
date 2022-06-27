wasm-pack:
	# wasm-pack build --release -t web --no-default-features --features browser
	cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
	wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm

test-core-wasm:
	wasm-pack test --chrome --features browser_chrome_test --no-default-features -p rings-core

test-browser:
	wasm-pack test --chrome --features browser_chrome_test --no-default-features
