SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
DIST_DIR ?= dist
PROJECT_ID ?=
GITLAB_TOKEN ?=
GITLAB_HOST ?= git.huayi.tech

UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
TARGET_OS := darwin
else ifeq ($(UNAME_S),Linux)
TARGET_OS := linux
else
$(error unsupported host OS: $(UNAME_S))
endif

ifeq ($(UNAME_M),arm64)
TARGET_ARCH := arm64
else ifeq ($(UNAME_M),aarch64)
TARGET_ARCH := arm64
else ifeq ($(UNAME_M),x86_64)
TARGET_ARCH := x86_64
else
$(error unsupported host architecture: $(UNAME_M))
endif

ARTIFACT_NAME := cx-$(TARGET_OS)-$(TARGET_ARCH)

.PHONY: help fmt test check build install clean clean_dist local_release \
	upload_release_assets release_links create_release

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

clean_dist: ## Remove packaged release artifacts
	rm -rf $(DIST_DIR)

clean: clean_dist ## Remove build outputs and packaged release artifacts
	cargo clean

local_release: clean_dist build ## Build and package a local release into dist/
	@mkdir -p $(DIST_DIR); \
	cp target/release/cx $(DIST_DIR)/$(ARTIFACT_NAME); \
	cp scripts/install-release.sh $(DIST_DIR)/install.sh; \
	chmod +x $(DIST_DIR)/install.sh; \
	if command -v sha256sum >/dev/null 2>&1; then \
		(cd $(DIST_DIR) && sha256sum * > SHA256SUMS); \
	else \
		(cd $(DIST_DIR) && shasum -a 256 * > SHA256SUMS); \
	fi; \
	printf 'Packaged %s\n' "$(DIST_DIR)/$(ARTIFACT_NAME)"; \
	printf 'Artifacts are in %s\n' "$(DIST_DIR)"

upload_release_assets: local_release ## Upload dist/* to GitLab Generic Package Registry
	@test -n "$(PROJECT_ID)" || { echo "set PROJECT_ID=<gitlab project id>"; exit 1; }
	@test -n "$(GITLAB_TOKEN)" || { echo "set GITLAB_TOKEN=<gitlab personal access token>"; exit 1; }
	@package_url="https://$(GITLAB_HOST)/api/v4/projects/$(PROJECT_ID)/packages/generic/cx/$(VERSION)"; \
	for file in $(DIST_DIR)/*; do \
		curl --fail --show-error --location \
			--header "PRIVATE-TOKEN: $(GITLAB_TOKEN)" \
			--upload-file "$$file" \
			"$$package_url/$$(basename "$$file")"; \
	done

release_links: local_release ## Generate dist/release-links.json for GitLab release assets
	@test -n "$(PROJECT_ID)" || { echo "set PROJECT_ID=<gitlab project id>"; exit 1; }
	@package_url="https://$(GITLAB_HOST)/api/v4/projects/$(PROJECT_ID)/packages/generic/cx/$(VERSION)"; \
	links='['; \
	sep=''; \
	for file in $(DIST_DIR)/cx-*; do \
		name="$$(basename "$$file")"; \
		links="$$links$$sep{\"name\":\"$$name\",\"url\":\"$$package_url/$$name\",\"filepath\":\"/binaries/$$name\",\"link_type\":\"package\"}"; \
		sep=','; \
	done; \
	if [[ -f $(DIST_DIR)/SHA256SUMS ]]; then \
		links="$$links$$sep{\"name\":\"SHA256SUMS\",\"url\":\"$$package_url/SHA256SUMS\",\"filepath\":\"/checksums/SHA256SUMS\",\"link_type\":\"other\"}"; \
		sep=','; \
	fi; \
	if [[ -f $(DIST_DIR)/install.sh ]]; then \
		links="$$links$$sep{\"name\":\"install.sh\",\"url\":\"$$package_url/install.sh\",\"filepath\":\"/install.sh\",\"link_type\":\"other\"}"; \
	fi; \
	links="$$links]"; \
	printf '%s\n' "$$links" > $(DIST_DIR)/release-links.json

create_release: release_links ## Create a GitLab release with glab using dist/release-links.json
	@test -n "$(GITLAB_TOKEN)" || { echo "set GITLAB_TOKEN=<gitlab personal access token>"; exit 1; }
	@command -v glab >/dev/null 2>&1 || { echo "glab is required for create_release"; exit 1; }
	@GITLAB_HOST="$(GITLAB_HOST)" GITLAB_TOKEN="$(GITLAB_TOKEN)" \
		glab release create "v$(VERSION)" \
			--name "Release v$(VERSION)" \
			--notes "Local release v$(VERSION)" \
			--assets-links "$$(cat $(DIST_DIR)/release-links.json)"
