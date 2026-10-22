# QuickStatements for a Wikibase of your own: the web service
# (`--command server`) and the bot that runs batches (`--command bot`).
# See docker-compose.yml and the README.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin main

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libcurl4 libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/main /usr/local/bin/quickstatements
# For temporary batches (`tmp_dir`)
RUN mkdir /tmp/quickstatements && chown nobody /tmp/quickstatements
USER nobody
ENTRYPOINT ["quickstatements", "--config-file", "/config/quickstatements.json"]
CMD ["--command", "server", "--port", "8000"]
