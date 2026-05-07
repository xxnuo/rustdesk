.PHONY: dev dev-up dev-down dev-build-image dev-shell dev-init dev-codegen dev-server dev-server-stop dev-logs dev-status dev-doctor build

COMPOSE_DEV := HOST_UID=$$(id -u) HOST_GID=$$(id -g) docker compose -f compose.dev.yml

dev: dev-up dev-server

dev-up:
	$(COMPOSE_DEV) up -d

dev-down:
	$(COMPOSE_DEV) down

dev-build-image:
	$(COMPOSE_DEV) build

dev-shell:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev shell

dev-init:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev init

dev-codegen:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev codegen

dev-server:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev server

dev-server-stop:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev server-stop

dev-logs:
	$(COMPOSE_DEV) exec rustdesk-dev tail -f /config/log/rustdesk-flutter.log

dev-status:
	$(COMPOSE_DEV) ps

dev-doctor:
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev doctor

build: dev-up
	$(COMPOSE_DEV) exec rustdesk-dev rustdesk-dev build
