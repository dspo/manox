# cx

`cx` 是一个基于 Rust TUI 的聚合入口，用来启动 `copilot`、`claude`、`codex`，并在真正进入原生 CLI 之前完成 Provider / Model 选择。

## 设计约定

- `cx`：先选择 agent，再选择 provider / model。
- `cx <agent> [args...]`：跳过 agent 选择，但仍会进入 provider / model 选择。
- 选择完成后，`[args...]` 会原样透传给底层原生 CLI。
- 不对 `mcp`、`plugin`、`skill` 等命令做特判，它们只是普通透传参数。
- 不代理 `codex app` 桌面端；如需桌面端请直接运行原生 `codex app ...`。

## 示例

```bash
cx
cx claude mcp list
cx codex --approval-mode on-request
cx probe
cx probe qwen3.6-plus
```

## 开发

```bash
./scripts/build.sh
cargo test
```

## 安装

```bash
./scripts/install.sh
```

安装脚本会：

1. 构建 release 版本的 `cx`
2. 安装到 `~/.local/bin/cx`
3. 移除 `~/.zshrc` 中旧的 `copilot` / `claude` / `codex` 劫持
4. 删除旧的 `~/.local/bin/ccc`
