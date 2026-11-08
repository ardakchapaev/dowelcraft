# dowel-server: headless dowel-craft with the HTTP API, for a Linux server.
# Build from the repository root:
#   docker build -f packaging/server/Dockerfile -t dowel-server .
# Run:
#   docker run -d --name dowel-server -p 127.0.0.1:8090:8090 -e DOWEL_TOKEN=<secret> -e DOWEL_LANG=ru dowel-server

FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p dowel-server

FROM debian:bookworm-slim
# DejaVu: Cyrillic glyphs for PDF cutting maps and drawings (pdf.rs looks for it in /usr/share/fonts).
RUN apt-get update \
 && apt-get install -y --no-install-recommends fonts-dejavu-core ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 dowel
COPY --from=build /src/target/release/dowel-server /usr/local/bin/dowel-server
USER dowel
EXPOSE 8090
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8090"]
ENTRYPOINT ["dowel-server"]
CMD ["serve", "--addr", "0.0.0.0:8090", "--threads", "4"]
