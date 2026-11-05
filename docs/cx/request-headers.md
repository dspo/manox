# 端点请求头（headers）配置

cx provider 配置支持为请求附加自定义 HTTP header。典型场景：上游要求
会话归因头（如 OpenCode Go 的 `x-opencode-session`）、网关鉴权、灰度
标记等——这些以前只能写死在代码里，现在由配置声明。

## 配置位置与合并规则

`headers` 可以出现在两个层级，渲染结果按 key 合并，**同名键 endpoint
级覆盖 provider 级**（key 按原文比较，区分大小写）：

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

注意：`responses: https://…` 这类 **plain URL 形式无法携带 headers**——
需要 headers 的 endpoint 必须写成 Detailed 形式（`url:` + 其余字段）。

## 模板语法

header 的**值**按 [Tera](https://tera.netlify.dev/) 模板渲染（与内置
prompt 模板同一引擎），每次请求（含重试）重新渲染。因此完整 Tera 语法
均可用——插值、过滤器、控制块皆可出现在值里。

### 目前支持的变量

| 变量 | 含义 |
|------|------|
| `session_id` | 当前会话的稳定 id（会话创建时生成的 UUID，全程不变；同一次请求的各次重试取值一致） |

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

### 转义行为

渲染时 **autoescape 关闭**：值按 wire 文本原样输出，`&`、`<` 等字符
不会被 HTML 转义。

## 运行时行为

- **渲染时机**：每次 provider 请求独立渲染；同一会话内 `session_id`
  取值稳定，天然满足上游「同一会话同一 key」的要求。
- **合并顺序**：provider 级先放入，endpoint 级覆盖同名键；随后追加
  运行时自身注入的鉴权头（如 `Authorization: Bearer …`），同名时以
  运行时为准。
- **配置往返**：设置页的模型表单目前没有 headers 编辑器，但保存时会
  **原样保留**手写的 `headers:`，不会被剥离。

## 真实案例：OpenCode Go

OpenCode Go 的推理端点要求每个请求携带稳定的会话归因头，缺失时返回
`400 MissingSessionID`。配置上面第一节的 `X-Opencode-Session` 后，
manox 对该 endpoint 的每次请求都会携带当前会话的 UUID，服务端即可按
会话路由与缓存。
