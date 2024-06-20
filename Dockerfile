# Temporary dev image
FROM alpine:3.20
ARG PACKAGE=k8s-managed-node-pool-do
RUN apk add --update musl-dev openssl-dev rust cargo

# Build and cache dependencies
WORKDIR /opt/app
COPY . .
RUN --mount=type=cache,target=./target \
    --mount=type=cache,target=/root/.cargo/ \
    cargo build --bin ${PACKAGE} --release &&\
    mkdir /opt/app/release &&\
    cp /opt/app/target/release/${PACKAGE} /opt/app/release/application

# Prod image
FROM alpine:3.20
ARG PACKAGE
RUN apk add --no-cache openssl
WORKDIR /opt/app
COPY --from=0 /opt/app/release/application .
CMD ["./application"]
