# cx internal edition

`cx` internal edition is a Rust TUI launcher for `copilot`, `claude`, and `codex`.
This edition is designed for GitLab-hosted internal distribution:

- provider/model config lives in runtime YAML files
- GitLab CI builds and publishes both rolling main artifacts and tag release assets
- the rolling GitLab install script is the primary installation path
- npm/npx is provided as a secondary GitLab-based distribution path

## Runtime model

- `cx`: choose agent first, then provider/model
- `cx <agent> [args...]`: skip agent selection, still choose provider/model
- after selection, passthrough args are forwarded unchanged to the native CLI
- for `codex`, `cx` injects a synthetic DashScope provider view before launch, so it does not depend on the user already having `~/.codex/config.toml`
- `cx` does **not** proxy `codex app`

## Stats dashboard

`cx stats` opens a token usage dashboard for reviewing model and agent usage.
The `Overview` view shows token trends and a model table with share, total
tokens, and per-agent breakdowns. The `Dynamicview` view adds an All time
cumulative token bar chart race for models, linked with the same model table as
the race date advances.

![cx stats Dynamicview](docs/dynamicview.gif)

## Install from GitLab

This is the primary installation path for internal users.

```bash
curl -fsSL \
  "https://git.huayi.tech/awesome/cx/-/jobs/artifacts/main/raw/dist/install.sh?job=publish-main" | sh
```

By default this installs the latest successful `main` build for the matching
platform, verifies `SHA256SUMS`, and writes `cx` to `~/.local/bin/cx`.

To install the latest stable release instead:

```bash
curl -fsSL \
  "https://git.huayi.tech/awesome/cx/-/jobs/artifacts/main/raw/dist/install.sh?job=publish-main" | \
  CX_CHANNEL=release sh
```

To install a specific release tag:

```bash
curl -fsSL \
  "https://git.huayi.tech/awesome/cx/-/jobs/artifacts/main/raw/dist/install.sh?job=publish-main" | \
  CX_CHANNEL=release CX_VERSION=v0.1.0 sh
```

The installer currently supports:

- `cx-linux-x86_64`
- `cx-linux-arm64`
- `cx-darwin-arm64`
- `cx-darwin-x86_64`

If `~/.local/bin` is not already in `PATH`, the installer prints the export line
to add before invoking `cx`.

## Install with npx

`npx` is available as a secondary GitLab-based path. If your environment can
reach the GitLab npm registry directly, install the wrapper and then let it
download the matching release binary.

```bash
npm config set @awesome:registry https://git.huayi.tech/api/v4/projects/<project-id>/packages/npm/
npm config set -- //git.huayi.tech/api/v4/projects/<project-id>/packages/npm/:_authToken=<gitlab-token>

npx @awesome/cx
```

## Runtime provider config

`cx` reads provider/model config from `~/.config/cx/cx.providers.config.yaml`
at runtime. If an older `~/.config/cx/config.yaml` exists, it is migrated to the
new path automatically on first use.

The repo keeps `config/providers.default.yaml` as the published baseline
reference. Typical workflows:

```bash
cx add
cx patch config/providers.default.yaml
cx patch --url <url>
cx patch --refresh
```

If `~/.config/cx/cx.providers.config.yaml` is missing, `cx` creates it from the
published baseline automatically on first use. You can also edit it directly.

`cx add` launches a TUI wizard rooted at the Providers list. From there you can:

- select an existing Provider, then add a `wire_api` endpoint or a model
- choose `+ 新建 Provider` to create a new API Provider, fill `apikey_source`,
  complete a three-row Anthropic / Responses / Completions endpoint form, and
  optionally add its first model

When a provider uses `apikey_source: keychain:<SERVICE>` and that secret is
missing, `cx` prompts on first real use and writes it back to Keychain. `env:`
sources are resolved strictly from the environment and are not rewritten.

For `codex`, only models verified to work through the injected DashScope
responses provider are exposed in the published baseline config.

## GitLab CI delivery

`.gitlab-ci.yml` provides:

1. `cargo fmt --check`
2. `cargo test`
3. build jobs for `cx-linux-x86_64`, `cx-linux-arm64`, `cx-darwin-arm64`, and `cx-darwin-x86_64`
4. a `publish-main` job whose raw artifacts provide a stable installer + checksum + binary URL for the latest successful `main` pipeline
5. tag pipelines upload versioned assets to the GitLab Generic Package Registry and create a GitLab Release with permanent asset links
6. tag pipelines publish the npm wrapper to the GitLab npm registry

## Local development

The repo pins Rust 1.95.0 with `rust-toolchain.toml` and CI runs the same
version.

```bash
./scripts/build.sh
cargo test
```

For a local binary install from source:

```bash
./scripts/install.sh
```
