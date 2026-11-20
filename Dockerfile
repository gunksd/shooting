# 守护进程镜像。控制台需要宿主机上的 baw 登录态，请在宿主机运行 console/。
FROM rust:1-bookworm AS build
WORKDIR /src
COPY shooter ./shooter
RUN cd shooter && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd -m shooter
COPY --from=build /src/shooter/target/release/shooter /usr/local/bin/shooter
USER shooter
WORKDIR /app
VOLUME /app/data
ENTRYPOINT ["shooter", "--config", "/app/config.toml"]
CMD ["run"]
