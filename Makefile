.PHONY: setup db dev dev-frontend seed-dev user-dev test lint fmt types sqlx-prepare check images up down reset ps logs psql seed user restore-drill e2e alerts-test loadtest loadtest-up loadtest-down loadtest-image

# One-time developer setup. Tools are not installed for you: missing ones are reported.
setup: ## Check tools, install npm deps, create the dev database
	@command -v sqlx >/dev/null || echo "missing sqlx-cli: cargo install sqlx-cli --no-default-features --features sqlite,rustls"
	@command -v docker >/dev/null || echo "missing docker (needed for tests that use Postgres and for the cluster)"
	@test -f .env || { cp .env.example .env; echo "created .env from .env.example"; }
	cd apps/frontend && npm ci
	$(MAKE) db

db: ## Create and migrate the dev database
	mkdir -p data
	sqlx database create
	sqlx migrate run --source migrations/sqlite

dev: ## Web app on :4000 against data/dev.db (no containers)
	cargo run -p library-web

dev-frontend: ## React dev server on :5173 (proxies /api to :4000)
	cd apps/frontend && npm run dev

seed-dev: ## Sample data in the dev database
	cargo run -p library-web -- seed

# Usage: make user-dev NAME=alice   (prompts for the password twice)
user-dev: ## Staff account in the dev database
	cargo run -p library-web -- create-user $(or $(NAME),admin)

# The second run is the web app's optional Postgres backend (load-test comparison only).
test: ## Rust tests, default build and web --features postgres; starts a throwaway Postgres (Docker)
	url=$$(./scripts/test-postgres.sh) && \
	TEST_POSTGRES_URL=$$url cargo test --workspace && \
	TEST_POSTGRES_URL=$$url cargo test -p library-web --features postgres

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy -p library-web --all-targets --features postgres -- -D warnings

fmt:
	cargo fmt

types: ## Regenerate TypeScript types from the shared crate
	cargo test -p library-shared --features ts

# sqlx-cli runs from apps/web/, so give it an absolute path to the dev DB.
sqlx-prepare: ## Refresh committed offline query metadata
	cd apps/web && DATABASE_URL=sqlite://$(CURDIR)/data/dev.db cargo sqlx prepare

# Same Prometheus image as deploy/compose/central.yml.
alerts-test: ## Check and unit-test the alert rules with promtool (Docker)
	docker run --rm -v "$(CURDIR)/deploy/monitoring:/m:ro" --entrypoint promtool prom/prometheus:v3.13.4 check rules /m/alerts.yml
	docker run --rm -v "$(CURDIR)/deploy/monitoring:/m:ro" --entrypoint promtool prom/prometheus:v3.13.4 test rules /m/alerts.test.yml

check: alerts-test ## Every CI gate except the Docker end-to-end test (make e2e)
	cargo fmt --check
	$(MAKE) lint
	$(MAKE) test
	SQLX_OFFLINE=true cargo build -p library-web -p sync-worker
	SQLX_OFFLINE=true cargo build -p library-web --features postgres
	@test -z "$$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)" || { echo "library-web must not depend on a Postgres driver"; exit 1; }
	cargo test -q -p library-shared --features ts && git diff --exit-code -- apps/frontend/src/generated
	cd apps/frontend && npm run lint && npm run typecheck && npm test

images: ## Build the three images (tag :dev)
	docker build -f deploy/docker/Dockerfile --target web -t sqlite-production/web:dev .
	docker build -f deploy/docker/Dockerfile --target sync-worker -t sqlite-production/sync-worker:dev .
	docker build -f deploy/docker/litestream.Dockerfile -t sqlite-production/litestream:dev .

loadtest-image: ## Build the web image with the Postgres backend (:lt), for the load-test comparison
	docker build -f deploy/docker/Dockerfile --target web --build-arg WEB_FEATURES=postgres -t sqlite-production/web:lt .

up: images ## Start Postgres, Garage, Prometheus and nodes 1-3 (ports 4001-4003)
	scripts/cluster.sh up

down: ## Stop the cluster; its data is kept
	scripts/cluster.sh down

reset: ## Stop the cluster and delete all its data (asks first)
	scripts/cluster.sh reset

ps: ## Show the cluster's containers
	scripts/cluster.sh ps

logs: ## Follow one node's logs: make logs NODE=node2
	scripts/cluster.sh logs $(or $(NODE),node1)

psql: ## psql on the cluster's Postgres
	docker compose -p central -f deploy/compose/central.yml exec postgres psql -U sync -d library

seed: ## Sample data on a cluster node: make seed NODE=node1
	scripts/cluster.sh exec $(or $(NODE),node1) web library-web seed

user: ## Staff account on a cluster node: make user NODE=node1 NAME=alice
	scripts/cluster.sh exec $(or $(NODE),node1) web library-web create-user $(or $(NAME),admin)

restore-drill: ## Restore a node's backup to a scratch copy and check it: make restore-drill NODE=node2
	scripts/restore-drill.sh $(or $(NODE),node1)

e2e: images ## End-to-end test on a separate, throwaway cluster
	scripts/test-restore-gate.sh
	scripts/e2e.sh

loadtest-up: ## Load test: start the 2 CPU / 4 GB node and seed it (needs make images)
	scripts/loadtest.sh up

loadtest-down: ## Load test: remove its cluster (results are kept)
	scripts/loadtest.sh down

loadtest: ## Load test: up, search users, search rps, down (takes hours; see docs/LOADTEST.md)
	scripts/loadtest.sh up
	scripts/loadtest.sh search users; rc=$$?; \
	scripts/loadtest.sh search rps || rc=$$?; \
	scripts/loadtest.sh down; exit $$rc
