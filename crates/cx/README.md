# cx internal edition

`cx` internal edition is a Rust TUI launcher for `copilot`, `claude`, and `codex`.
It is maintained inside the manox repository (`crates/cx` of
`github.com/dspo/manox`):

- provider/model config lives in runtime YAML files
- built and installed from source with `cargo install` (see below)

## Runtime model

- `cx`: choose agent first, then provider/model
- `cx <agent> [args...]`: skip agent selection, still choose provider/model
- after selection, passthrough args are forwarded unchanged to the native CLI
- for `codex`, `cx` injects a synthetic DashScope provider view before launch, so it does not depend on the user already having `~/.codex/config.toml`
- `cx` does **not** proxy `codex app`

## Stats dashboard

`cx stats` opens a token usage dashboard for reviewing model and agent usage.
The `Overview` view shows token trends and a model table with share, total
tokens, and per-agent breakdowns. The `All Time Race` view adds an All time
cumulative token bar chart race for models, linked with the same model table
as the race date advances. The `7-Days Rolling Race` view shows a 7-day
rolling total token bar chart race, useful for surfacing recent model
preference shifts that get masked by the cumulative view's first-mover
advantage.

![cx stats All Time Race](../../docs/cx/dynamicview.gif)

## Install

From the manox repository root:

```bash
cargo install --path crates/cx --root ~/.local/ --locked
```

This builds the release binary and installs it to `~/.local/bin/cx`. If
`~/.local/bin` is not already in `PATH`, add it before invoking `cx`.

Alternatively, `make -C crates/cx install` (or `crates/cx/scripts/install.sh`)
builds the release binary and installs it to the same location.

## Runtime provider config

`cx` reads provider/model config from `~/.manox/cx.providers.config.yaml`
at runtime. If an older `~/.manox/config.yaml` exists, it is migrated to the
new path automatically on first use.

The repo keeps `config/providers.default.yaml` as the published baseline
reference. Typical workflows:

```bash
cx add
cx patch config/providers.default.yaml
cx patch --url <url>
cx patch --refresh
```

If `~/.manox/cx.providers.config.yaml` is missing, `cx` creates it from the
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

## Local development

The manox repo pins Rust 1.95.0 with `rust-toolchain.toml` and CI runs the
same version.

```bash
./scripts/build.sh
cargo test
```

For a local binary install from source:

```bash
./scripts/install.sh
```
