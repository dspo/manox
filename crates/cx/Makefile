SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

.PHONY: help fmt test check build install clean

help: ## Show available targets
	@grep -E '^[a-zA-Z0-9_%-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "}; {printf "\033[36m%-22s\033[0m %s\n", $$1, $$2}'

fmt: ## Run rustfmt check
	cargo fmt --check

test: ## Run cargo tests
	cargo test

check: fmt test ## Run formatting and tests

build: ## Build the release binary
	cargo build --release

install: ## Install cx to ~/.local/bin via scripts/install.sh
	./scripts/install.sh

clean: ## Remove build outputs
	cargo clean
