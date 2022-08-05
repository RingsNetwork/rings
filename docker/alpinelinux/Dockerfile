FROM rustlang/rust:nightly-alpine as builder
RUN apk add build-base pkgconf openssl-dev
WORKDIR  /project
ARG GIT_SHORT_HASH=
ENV GIT_SHORT_HASH=${GIT_SHORT_HASH}
ADD . /project
RUN cargo build --release

FROM alpine:3.15
RUN apk add ca-certificates tzdata
WORKDIR /app
COPY --from=builder \
    /project/target/release/rings \
    /app/rings
ENTRYPOINT ["/app/rings"]
