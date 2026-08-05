# Codex2.app 硬编码客制化记录

## 目标

把 `Codex2.app` 固化成一个可双击启动、默认走 DashScope，并在模型列表里显示：

- `qwen3.7-max`
- `qwen3.7-plus`

且不依赖外部启动器。

## 当前落地状态

### App 内置环境变量

`/Applications/Codex2.app/Contents/Info.plist`

- `CFBundleDisplayName = Codex2`
- `CFBundleIdentifier = com.openai.codex2`
- `LSEnvironment.CODEX_HOME = /Users/chenzhongrun/.config/cx/codex2`

注意：

- 这里只固化 `CODEX_HOME`
- 不要再伪造 `HOME`

原因是 `HOME` 会影响 keychain、桌面端用户态缓存和部分会话可见性，副作用太大。

## 当前 profile 目录

`/Users/chenzhongrun/.config/cx/codex2`

其中关键配置是：

- `config.toml`
- `dashscope-model-catalog.json`
- sqlite 状态库
- sessions / plugins / skills / electron-user-data

当前 `config.toml` 的核心字段：

```toml
model = "qwen3.7-plus"
model_provider = "dashscope"
model_catalog_json = "/Users/chenzhongrun/.config/cx/codex2/dashscope-model-catalog.json"
model_reasoning_effort = "high"

[model_providers.dashscope]
name = "DashScope"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
env_key = "DASHSCOPE_API_KEY"
wire_api = "responses"
```

## 仅改配置为什么不够

单改 `config.toml` 只能让后端请求发到 DashScope。

桌面端模型下拉列表是否显示正确，还受 renderer 内部模型查询逻辑控制。也就是说：

- “实际请求模型” 和
- “UI 下拉列表显示什么”

是两条不同链路。

## 这次真正改动了哪里

我们直接 patch 了 `app.asar` 解包后的 3 个 renderer 文件：

- `webview/assets/model-queries-DmmJqKhY.js`
- `webview/assets/model-list-filter-BOpqDcyc.js`
- `webview/assets/models-and-reasoning-efforts-Ct6D5g-X.js`

### 1. `model-queries-DmmJqKhY.js`

这里负责把 Statsig、host models、默认模型合并起来。

这次补丁做了几件事：

- 把 `qwen3.7-max`、`qwen3.7-plus` 注入 `available_models`
- 把这两个模型注入 `list-models-for-host` 的结果
- 把默认模型 fallback 固定成 `qwen3.7-plus`
- 把 `displayName` 统一回落到模型 id

### 2. `model-list-filter-BOpqDcyc.js`

这里负责 UI 层最终可见列表。

这次补丁做了几件事：

- 硬插入 `qwen3.7-max`、`qwen3.7-plus`
- 强制 `hidden = false`
- 强制 `isHidden = false`
- 默认 `displayName = id`
- 当 host 没给默认模型时，回落到 `qwen3.7-plus`

### 3. `models-and-reasoning-efforts-Ct6D5g-X.js`

这里负责默认模型和默认 reasoning effort。

这次补丁把默认值改成：

- `default model = qwen3.7-plus`
- `default reasoning effort = high`

## 为什么这能让列表显示真实 model id

因为真正控制显示的并不是 provider 配置本身，而是 renderer 最终拿到的模型描述符。

只要最终传给 dropdown 的对象里：

- `id = "qwen3.7-max"`
- `displayName = "qwen3.7-max"`

那 UI 就会显示这个真实 id，而不是 `GPT-5`。

## 无需外部启动器的关键

关键不是 shell 包一层，而是把这两件事固化进 App 本体：

1. `Info.plist` 里固定 `CODEX_HOME`
2. `app.asar` 里固定模型列表逻辑

这样双击 `/Applications/Codex2.app` 即可启动到目标 profile。

## DASHSCOPE_API_KEY 的要求

`config.toml` 使用的是：

```toml
env_key = "DASHSCOPE_API_KEY"
```

因此 Finder 双击启动时，macOS 登录会话必须能拿到这个环境变量。

推荐做法是把它注入用户级 `launchctl` 环境，而不是只写进某个交互 shell：

```bash
launchctl setenv DASHSCOPE_API_KEY '你的密钥'
```

## 复刻步骤

### 1. 解包 app.asar

```bash
npx asar extract /Applications/Codex2.app/Contents/Resources/app.asar /tmp/codex2_asar_full
```

### 2. 修改 renderer 资源

修改这 3 个文件：

- `webview/assets/model-queries-DmmJqKhY.js`
- `webview/assets/model-list-filter-BOpqDcyc.js`
- `webview/assets/models-and-reasoning-efforts-Ct6D5g-X.js`

核心目标只有三个：

- 列表里并入目标 Qwen 模型
- 默认模型回落到 `qwen3.7-plus`
- `displayName` 直接使用模型 id

### 3. 重新打包

```bash
npx asar pack /tmp/codex2_asar_full /tmp/codex2_app_patched.asar
```

### 4. 替换并重签名

```bash
cp /Applications/Codex2.app/Contents/Resources/app.asar /Applications/Codex2.app/Contents/Resources/app.asar.bak.$(date +%s)
cp /tmp/codex2_app_patched.asar /Applications/Codex2.app/Contents/Resources/app.asar
codesign --force --deep --sign - /Applications/Codex2.app
```

### 5. 固化 CODEX_HOME

```bash
/usr/libexec/PlistBuddy -c 'Set :LSEnvironment:CODEX_HOME /Users/chenzhongrun/.config/cx/codex2' /Applications/Codex2.app/Contents/Info.plist
codesign --force --deep --sign - /Applications/Codex2.app
```

## 当前这版的局限

这是 build-specific patch，不是通用扩展点。

具体来说：

- 文件名 hash 会随上游版本变化
- Statsig / renderer 内部逻辑随版本可能改变
- 每次官方更新 App 后，可能都要重新定位并 patch

所以它适合作为：

- POC
- 反向规格说明
- 临时可用版本

不适合作为 `cx` 的长期架构。

## 给 cx 的启发

这次 patch 本质上接管了 3 件事：

1. 模型列表来源
2. 默认模型决策
3. 模型显示名策略

`cx` 的正式重构，应该把这 3 件事前移到 host / config 层，而不是继续改打包后的 renderer 产物。
