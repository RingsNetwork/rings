GIT_COMMIT := $(shell git rev-parse --short HEAD)
wasm-pack:
	# wasm-pack build --release -t web --no-default-features --features browser
	cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
	wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm

build-browser-pack:
	wasm-pack build --out-dir target/browser/rings-node --scope ringsnetwork -t web --no-default-features --features browser
test-core-wasm:
	wasm-pack test --chrome --features browser_chrome_test --no-default-features -p rings-core

test-browser:
	wasm-pack test --chrome --features browser_chrome_test --no-default-features

build-docker-image:
	docker build --build-arg GIT_SHORT_HASH=$(GIT_COMMIT) -t rings-network/rings-node -f ./docker/alpinelinux/Dockerfile ./
