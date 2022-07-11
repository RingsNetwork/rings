FROM rustlang/rust:nightly-alpine as builder
RUN apk add build-base pkgconf openssl-dev
WORKDIR  /project
ADD . /project
RUN cargo build --release --features client --no-default-features

FROM alpine:3.15
RUN apk add ca-certificates tzdata
WORKDIR /app
COPY --from=builder \
    /project/target/release/rings-node \
    /app/rings-node
ENTRYPOINT ["/app/rings-node"]

