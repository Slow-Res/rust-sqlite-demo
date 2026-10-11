.PHONY: setup db dev dev-frontend seed-dev user-dev test lint fmt types sqlx-prepare check

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

test: ## Rust tests; starts a throwaway Postgres (Docker) for the worker tests
	TEST_POSTGRES_URL=$$(./scripts/test-postgres.sh) cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt

types: ## Regenerate TypeScript types from the shared crate
	cargo test -p library-shared --features ts

# sqlx-cli runs from apps/web/, so give it an absolute path to the dev DB.
sqlx-prepare: ## Refresh committed offline query metadata
	cd apps/web && DATABASE_URL=sqlite://$(CURDIR)/data/dev.db cargo sqlx prepare

check: ## Every gate CI runs
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
	$(MAKE) test
	SQLX_OFFLINE=true cargo build -p library-web
	cd apps/frontend && npm run lint && npm run typecheck && npm test
