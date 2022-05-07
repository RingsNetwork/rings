wasm-pack:
	# wasm-pack build --release -t web --no-default-features --features browser
	cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
	wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm

