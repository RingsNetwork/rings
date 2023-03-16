GIT_COMMIT := $(shell git rev-parse --short HEAD)
build-docker-image:
	docker build --build-arg GIT_SHORT_HASH=$(GIT_COMMIT) -t rings-network/rings-node -f ./docker/alpinelinux/Dockerfile ./

fmt:
	cargo +nightly fmt
