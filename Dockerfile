FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -m aiws && mkdir /data && chown aiws /data
COPY --from=build /src/target/release/aiws /usr/local/bin/aiws
USER aiws
ENV AIWS_HOME=/data
VOLUME /data
EXPOSE 7878
# The daemon refuses to listen on a non-loopback address without AIWS_TOKEN.
ENTRYPOINT ["aiws"]
CMD ["serve", "--bind", "0.0.0.0:7878"]
