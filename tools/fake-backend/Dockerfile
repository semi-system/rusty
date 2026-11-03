FROM rust:1.99.1-alpine3.22@sha256:0fd356c1b3a83a6a6a2a4113ede8166eac6697a663fa822c04f0c202e63fcc01 AS build
ENV RUSTUP_TOOLCHAIN=1.99.1
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p fake-backend && cp /src/target/release/fake-backend /fake-backend

FROM scratch
COPY --from=build /fake-backend /fake-backend
USER 65532:65532
ENTRYPOINT ["/fake-backend"]
