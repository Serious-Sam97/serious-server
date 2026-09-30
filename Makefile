.PHONY: build frontend backend dev install clean docker docker-dev

DOCKER_GID := $(shell getent group docker | cut -d: -f3)

# Production container (published to host loopback only).
docker:
	DOCKER_GID=$(DOCKER_GID) docker compose up -d --build
	@echo "First boot? Get the setup token with: docker compose logs serious-server"

# Hot-reload dev containers: cargo-watch backend + vite HMR on :5173.
docker-dev:
	docker compose -f docker-compose.dev.yml up

build: frontend backend

frontend:
	cd frontend && npm run build

backend:
	cd backend && cargo build --release

# Run backend + vite dev server side by side (vite proxies /api to :8420).
dev:
	(cd backend && SS_COOKIE_SECURE=false cargo run) & \
	(cd frontend && npm run dev) & \
	wait

install: build
	mkdir -p ~/.config/systemd/user
	cp deploy/serious-server.service ~/.config/systemd/user/
	systemctl --user daemon-reload
	systemctl --user enable --now serious-server
	@echo "Run 'loginctl enable-linger $$USER' once so it survives logout."

clean:
	cd backend && cargo clean
	rm -rf frontend/dist
