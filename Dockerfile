# ---- frontend build ----
FROM node:24-alpine AS frontend
WORKDIR /app/frontend
COPY frontend/package*.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# ---- backend build ----
# -bookworm pinned to match the runtime image's glibc
FROM rust:1.94-slim-bookworm AS backend
WORKDIR /app/backend
COPY backend/ ./
# rust-embed resolves ../frontend/dist relative to the backend crate
COPY --from=frontend /app/frontend/dist /app/frontend/dist
RUN cargo build --release

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash git openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 sam
# docker CLI + compose plugin, for project-level compose actions against the
# host daemon via the mounted socket
COPY --from=docker:28-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker:28-cli /usr/local/libexec/docker/cli-plugins/docker-compose /usr/local/libexec/docker/cli-plugins/docker-compose
COPY --from=backend /app/backend/target/release/serious-server /usr/local/bin/serious-server

ENV SHELL=/bin/bash \
    SS_BIND=0.0.0.0:8420 \
    SS_ALLOW_PUBLIC_BIND=true \
    SS_DATA_DIR=/data
USER sam
EXPOSE 8420
ENTRYPOINT ["serious-server"]
