# 端点请求头（headers）配置

cx provider 配置支持为请求附加自定义 HTTP header。典型场景：上游要求
会话归因头（如 OpenCode Go 的 `x-opencode-session`）、网关鉴权、灰度
标记等——这些以前只能写死在代码里，现在由配置声明。

## 配置位置与合并规则

`headers` 可以出现在两个层级，渲染结果按 header 名合并，**同名键 endpoint
级覆盖 provider 级**（header 名大小写不敏感，`X-Shared` 与 `x-shared`
是同一个键）：

```yaml
providers:
  - name: OpenCode Go
    apikey_source: keychain:OPENCODE_GO_APIKEY
    headers:                      # provider 级：进入本 provider 全部 endpoint
      X-Trace-Source: manox
    endpoints:
      anthropic:
        url: https://opencode.ai/inference/go/anthropic
        headers:                  # endpoint 级：仅该 endpoint
          X-Opencode-Session: "{{ session_id }}"
      completions:
        url: https://opencode.ai/inference/go/v1
```

注意：`responses: https://…` 这类 **plain URL 形式无法携带 endpoint 级
headers**（仍会继承 provider 级）——需要自身 headers 的 endpoint 必须写成
Detailed 形式（`url:` + 其余字段）。

## 模板语法

header 的**值**按 [Tera](https://tera.netlify.dev/) 模板渲染（与内置
prompt 模板同一引擎）。每个 provider 请求渲染一次，同一次请求的各次
重试复用同一结果。因此完整 Tera 语法均可用——插值、过滤器、控制块皆可
出现在值里。

### 目前支持的变量

| 变量 | 含义 |
|------|------|
| `session_id` | 当前会话的稳定 id（会话创建时生成的 UUID，全程不变；重开会话仍是同一个 id） |

上下文里**只有这一个变量**。示例：

```yaml
headers:
  X-Opencode-Session: "{{ session_id }}"        # → 9f0c2e0a-…
  X-Cache-Tag: "sess-{{ session_id | truncate(length=8) }}"
```

`{{ }}` 内的空白不敏感（`{{session_id}}` 等价）。过滤器是 Tera 内置
滤镜全集（`truncate` / `upper` / `trim` …），参数写法遵循 Tera 文档。

### 失败语义：整条丢弃

header 不是容错字段——只要渲染不成功，该条 header **整体丢弃**并记录
一条 warn 日志，绝不会把模板字符串原样发到上游：

- 引用了上下文里不存在的变量（如 `{{ model }}`）→ 丢弃
- 请求没有 `session_id` 而模板引用了它 → 丢弃
- 括号不闭合、`{%` 块语法错误等解析失败 → 丢弃

设计意图：header 值一旦渲染失败，发送残缺值（空串、半个模板）几乎
必然触发上游报错且更难排查；宁可让请求以「缺 header」的明确形态到达。

配置加载时还会先做一次预渲染校验：永远渲染不出来的模板在启动时就打一条
warn（带上 provider / endpoint / header 名），不必等第一次请求。

### 转义行为

渲染时 **autoescape 关闭**：值按 wire 文本原样输出，`&`、`<` 等字符
不会被 HTML 转义。

## 运行时行为

- **渲染时机**：每次 provider 请求渲染一次；同一会话内 `session_id`
  取值稳定，天然满足上游「同一会话同一 key」的要求。
- **合并顺序**：provider 级先放入，endpoint 级覆盖同名键（大小写不敏感，
  每个名字最终只剩一个值）；随后追加运行时自身注入的鉴权头（如
  `Authorization: Bearer …`），同名时以运行时为准。
- **provider 自有的协议头不可配置**：`Authorization`、`x-api-key`、
  `content-type`、`anthropic-version` 由 provider 客户端自己写入，配置里
  同名条目会被丢弃（不会在 wire 上出现两个同名头）。
- **顺带传出的缓存 key**：开启 prompt caching 时，`session_id` 同时作为
  OpenAI 形状的 `prompt_cache_key` 发出（completions 仅对官方
  `api.openai.com` 端点，responses 对所有实现了该字段的端点；关闭
  caching 即不发送）。
- **配置往返**：设置页的模型表单目前没有 headers 编辑器，但保存时会
  **原样保留**手写的 `headers:`，不会被剥离。

## 真实案例：OpenCode Go

OpenCode Go 的推理端点要求每个请求携带稳定的会话归因头，缺失时返回
`400 MissingSessionID`。配置上面第一节的 `X-Opencode-Session` 后，
manox 对该 endpoint 的每次请求都会携带当前会话的 UUID，服务端即可按
会话路由与缓存。
