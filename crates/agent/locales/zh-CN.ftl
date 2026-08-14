## manox UI strings — 简体中文 (zh-CN)。
##
## 仅 UI chrome；模型面向字符串（system prompt / 工具 description / 工具错误）一律英文，不经此栈。
## 缺失键回退到 en.ftl。

### sidebar.rs
sidebar-new-chat = 新对话
sidebar-section-projects = 项目
sidebar-section-conversations = 对话
sidebar-new-session-label = 新建会话
sidebar-new-session-manox = Manox
session-kind-terminal = 终端
sidebar-close-external = 关闭会话
sidebar-resume-external = 恢复会话
external-session-resume-failed = 恢复外部 agent 失败
sidebar-archive = 归档
external-wizard-no-model = 尚无模型支持该 agent
external-session-start-failed = 启动外部 agent 失败
plain-session-start-failed = 启动终端会话失败
sidebar-empty-summary = (新对话)
sidebar-copy-thread-id = 复制 thread id
sidebar-pending-auth = 等待审批
sidebar-time-just-now = 刚刚
sidebar-time-minutes = {$count} 分钟前
sidebar-time-hours = {$count} 小时前
sidebar-time-days = {$count} 天前
sidebar-time-weeks = {$count} 周前

### message.rs
message-reasoning = 思考
message-error = 错误
message-notice = 通知
message-team = 团队消息
member-editor-tab = 编辑器
browser-tab = { $url }
browser-address-placeholder = 输入网址
browser-yield-hint = 已让出控制权（例如用于登录）。完成后请点此。
browser-yield-complete = 完成
browser-read-hint = Agent 正在读取本页 —— 页面中已登录的内容将暴露给 agent。
message-user-role = 你
recap-card-title = 上下文已压缩
cache-miss-label = 缓存未命中 · { $tokens } tokens
retry-badge = 重试中… { $attempt }/{ $max } · { $secs }秒 · { $reason }
message-omitted-prefix = …（已省略前面部分）
status-pending = 待审批
status-running = 运行中
status-success = 完成
status-continued = 继续讨论
status-error = 出错
status-denied = 已拒绝
status-cancelled = 已取消

### views/message.rs — Thinking 状态行
context-agents-title = 智能体
context-agents-captain = 船长

### views/subagent_panel.rs
subagent-panel-waiting = 等待子代理活动…
subagent-panel-final-note = 实时转录不跨重载保留；以下为子代理的最终回答。
plan-card-title = 计划
plan-card-download = 下载计划
plan-card-copy = 复制计划
plan-verdict-execute-fresh = 批准并新上下文执行
plan-verdict-execute-compact = 批准并压缩上下文执行
plan-verdict-execute-keep = 批准并保留上下文执行
plan-verdict-refine = 修订计划
plan-refine-notice = Plan 模式保持开启——输入你的修改意见，模型将更新 plan 文件并重新提交。
plan-chip-label = Plan 模式
plan-mode-on-notice = Plan 模式已开启：工作树只读；模型调研、写 plan 文件，并通过 ProposePlan 提交给你批准。
plan-mode-off-notice = Plan 模式已关闭：完整写权限恢复。
thinking-tool-result = 工具结果
thinking-reading = 读取 { $count } 个文件
thinking-writing = 写入 { $count } 个文件
thinking-editing = 编辑 { $count } 个文件
thinking-running = 运行 { $count } 条命令
thinking-fetching = 抓取 { $count } 个页面
thinking-browsing = 浏览器 { $count } 个动作
thinking-searching = 搜索 { $count } 个模式
thinking-globbing = 匹配 { $count } 个 glob
thinking-listing = 列出 { $count } 个目录
thinking-other = { $count } 个其他工具
thinking-rounds = 思考了 { $count } 轮次
thinking-tool-calls = 调用了 { $count } 次工具
thinking-duration = { $count } 秒

### views/settings.rs
settings-group-general = 通用
settings-item-general = 常规
settings-item-appearance = 外观
settings-item-config = 配置
settings-item-models = 模型
settings-item-personalization = 个性化
settings-item-pets = 宠物
settings-item-keyboard = 键盘快捷键
settings-group-integrations = 集成
settings-item-snapshots = 应用快照
settings-item-plugins = 插件
settings-item-browser = 浏览器
settings-item-computer = 电脑操控
settings-group-coding = 编码
settings-item-hooks = 钩子
settings-item-connections = 连接
settings-item-git = Git
settings-item-environment = 环境
settings-item-worktrees = 工作树
settings-group-external-tools = 外部工具
settings-item-chatgpt-app = ChatGPT.app
settings-item-vscode-app = Visual Studio Code.app
settings-group-archived = 已归档
settings-item-archived = 已归档对话
settings-item-chat-settings = 聊天设置
settings-search-placeholder = 搜索设置…
settings-back = 返回应用
settings-title = 设置
settings-coming-soon = Coming soon…
settings-coming-soon-label = Coming soon… {$label}

### views/settings.rs — 常规面板
settings-section-work-mode = 工作模式
settings-desc-work-mode = 选择 manox 显示多少技术细节
settings-row-work-mode-programming = 适用于编程
settings-desc-work-mode-programming = 更具技术性的回复和控制
settings-row-work-mode-workday = 适用于日常工作
settings-desc-work-mode-workday = 同样强大，技术细节更少

settings-section-permissions = 权限
settings-row-permission-autopilot = 自动驾驶
settings-desc-permission-autopilot = manox 可以读取和编辑其工作区中的文件。安全审查器自动批准额外访问权限请求。自动审批可能会出错。
settings-row-permission-danger = 危险驾驶
settings-desc-permission-danger = 当 manox 以危险驾驶模式运行时，无需你批准，即可编辑你的电脑上的任何文件并运行互联网命令。这会显著增加数据丢失、泄露或意外行为的风险。
settings-link-learn-more = 了解更多

settings-section-general-misc = 常规
settings-row-file-target = 默认文件打开目标
settings-desc-file-target = 默认打开文件和文件夹的位置
settings-row-ui-language = 用户界面语言
settings-desc-ui-language = 界面显示语言，保存后立即生效。
settings-row-agent-language = 对话语言
settings-desc-agent-language = 新建对话使用的语言。已有对话保持创建时的语言。
settings-save-failed-title = 设置保存失败
settings-saved = 已保存

# 外部工具 → ChatGPT.app 面板
settings-panel-chatgpt-app = ChatGPT.app
settings-desc-chatgpt-top = 配置 cx 启动 ChatGPT.app 时注入的内容。
settings-btn-copy = 复制
settings-btn-reveal = 在 Finder 中显示
settings-section-chatgpt-home = Codex Home
settings-desc-chatgpt-home = ChatGPT.app 对话管理主路径，用于与官方路径隔离。
settings-section-chatgpt-injection = 模型注入
settings-desc-chatgpt-injection = 决定自定义模型在 ChatGPT.app 中的可用方式。
settings-row-chatgpt-nickname = 昵称
settings-desc-chatgpt-nickname = 配置后，启动 ChatGPT.app 时以该昵称替换注入的 provider 名称（无论选用哪个 provider）。
settings-chatgpt-nickname-ph = 选填
settings-row-chatgpt-injection = 模型注入方式
settings-value-injection-list = 模型列表
settings-value-injection-single = 单个模型
settings-desc-chatgpt-injection-risk = 注入模型列表须使用 CDP 机制将模型列表注入 ChatGPT.app 进程运行时，选择即表示接受风险。
settings-row-chatgpt-providers = Providers & LLMs
settings-chatgpt-models-loading = 正在加载模型目录…
settings-chatgpt-models-load-failed = 加载失败：{ $error }
settings-chatgpt-models-empty = 未找到可注入的模型（需要支持 Responses 的端点）。
settings-section-chatgpt-env = 变量注入
settings-desc-chatgpt-env = 启动 ChatGPT.app 时的额外变量。
settings-chatgpt-env-key-ph = 变量名
settings-chatgpt-env-value-ph = 值
settings-btn-add-env = 添加变量
settings-section-chatgpt-more = 更多配置
settings-desc-chatgpt-more = 写入注入 config.toml 的高级选项。
settings-desc-chatgpt-websockets = 写入注入的 [model_providers.*] 段，默认 false（自定义端点普遍不支持 WebSocket 流式，走 HTTP 流式）。端点支持 WebSocket 流式时可设为 true。

# 外部工具 → Visual Studio Code.app 面板
settings-panel-vscode-app = Visual Studio Code.app
settings-desc-vscode-top = 从 Manox.app 启动 Visual Studio Code 时，为其 Claude Code 拓展和 Codex 拓展配置 Provider 和 LLM。
settings-section-vscode-claude = Claude Code Extension
settings-section-vscode-codex = Codex Extension
settings-row-vscode-provider = Provider
settings-vscode-no-inject = 不注入
settings-vscode-models-loading = 正在加载模型目录…
settings-vscode-models-load-failed = 加载失败：{ $error }
settings-vscode-models-empty-anthropic = 未找到可注入的模型（需要支持 Anthropic 协议的端点）。
settings-vscode-models-empty-responses = 未找到可注入的模型（需要支持 Responses 的端点）。
settings-row-menu-bar = 在菜单栏中显示
settings-desc-menu-bar = 关闭窗口后，仍在 macOS 菜单栏中保留 manox
settings-row-bottom-panel = 底部面板
settings-desc-bottom-panel = 在应用标题栏中显示底部面板控件
settings-row-terminal-location = 默认终端位置
settings-desc-terminal-location = 选择终端快捷键和环境操作在何处打开终端标签页
settings-row-keep-awake = 运行时防止休眠
settings-desc-keep-awake = 在 manox 运行聊天时，保持电脑唤醒
settings-row-code-review = 代码审查
settings-desc-code-review = 尽可能在当前对话中启动 /review，或发起单独的审查对话
settings-row-import = 从其他 AI 应用导入工作内容
settings-desc-import = 导入您的设置、项目和最近聊天记录
settings-row-licenses = 打开源许可证
settings-desc-licenses = 捆绑依赖项的第三方声明
settings-btn-import = 导入
settings-btn-view = 查看
settings-value-vscode = VS Code
settings-value-bottom = 底部
settings-value-right = 右侧
settings-value-inline = 行内视图
settings-value-detached = 分离视图

settings-section-editor = 编辑器
settings-row-send-shortcut = 发送快捷键
settings-desc-send-shortcut = 选择 Enter 何时发送提示或插入新行
settings-value-enter-shift = ⌘ + Enter for multiline prompts

settings-section-pop-up = 弹出窗口
settings-row-pop-up-shortcut = 弹出窗口快捷键
settings-desc-pop-up-shortcut = 为弹出窗口设置全局快捷键。留空则保持关闭
settings-value-disabled = 禁用
settings-value-configured = 设置
settings-btn-set = 设置
settings-row-default-no-project = 默认使用无项目聊天
settings-desc-default-no-project = 无需项目即可开始新聊天

settings-section-dictation = 听写
settings-row-microphone = 麦克风
settings-desc-microphone = 用于听写
settings-value-system-default = 系统默认
settings-row-press-dictate = 按住听写快捷键
settings-desc-press-dictate = 在桌面任意位置按住，即可在光标处听写
settings-row-toggle-dictate = 切换听写快捷键
settings-desc-toggle-dictate = 在桌面任意位置按一次开始听写，再按一次停止
settings-row-keep-dictation-bar = 保持听写栏可见
settings-desc-keep-dictation-bar = 听写未激活时显示小型快捷键提醒
settings-value-off = 关闭
settings-value-on = 开启

settings-section-notifications = 通知
settings-row-turn-completion = 轮次完成通知
settings-desc-turn-completion = 设置 manox 完成任任务时的提醒
settings-value-focus-only = 仅当应用失焦时
settings-row-permission-notify = 启用权限通知
settings-desc-permission-notify = 在需要通知权限时显示提醒
settings-row-question-notify = 启用问题通知
settings-desc-question-notify = 需要输入才能继续时显示提醒

### views/settings.rs — 配置面板
settings-panel-config = 配置
settings-desc-config-top = 配置审批策略和沙盒设置
settings-section-config-toml = 自定义 config.toml 设置
settings-row-config-user = 用户配置
settings-link-open-config = 打开 config.toml
settings-row-config-approval = 批准策略
settings-desc-config-approval = 选择 manox 何时请求批准
settings-value-on-request = 按请求
settings-row-config-sandbox = 沙盒设置
settings-desc-config-sandbox = 选择 manox 的命令执行权限
settings-value-read-only = 只读

settings-section-workspace-deps = 工作空间依赖项
settings-row-config-version = 当前版本
settings-btn-diagnose = 🔍 诊断
settings-desc-config-diagnose = 检查当前捆绑包并记录诊断日志
settings-row-config-builtin-deps = 内置依赖项
settings-desc-config-builtin-deps = 允许 manox 安装并提供随附的 Node.js 和 Python 工具
settings-row-config-reinstall = 重置并安装工作空间
settings-desc-config-reinstall = 删除本地捆绑包，重新下载后重新加载工具
settings-btn-reinstall = 重新安装

### views/settings.rs — 模型面板
settings-models-add-provider = 添加 Provider
settings-models-no-path = 无法解析 Provider 配置路径
settings-models-reload-failed-title = Provider 重载失败
settings-models-empty = 尚未配置任何 Provider
settings-models-unnamed = 未命名 Provider
settings-models-no-selection = 选择左侧 Provider 查看模型
settings-models-load-error-title = 配置读取失败
settings-models-load-error-hint = 为避免覆盖无法识别的文件，自动保存已禁用。请修正文件后重新打开本面板。
settings-models-ph-name = Provider 显示名称
settings-models-section-basic = 基本信息
settings-models-row-apikey = API Key
settings-models-apikey-literal = 字面值
settings-models-apikey-env = 环境变量
settings-models-apikey-keychain = Keychain
settings-models-apikey-shell = shell 命令
settings-models-section-env = 环境变量
settings-models-section-endpoints = 端点配置
settings-models-add-endpoint = 添加端点
settings-models-row-url = URL
settings-models-ph-url = https://api.example.com
settings-models-agents-all-hint = 未选择 = 全部 Agents
settings-models-agents-add = 添加
settings-models-row-copilot = GitHub Copilot 认证方式
settings-models-env-empty = 暂无环境变量
settings-models-empty-models = 暂无模型
settings-models-value-unset = 未设置
settings-models-section-models = 模型列表
settings-models-mode-inline = 手动配置
settings-models-mode-remote = 自动获取
settings-models-ph-remote-url = https://api.example.com/v1/models
settings-models-row-model-id = 模型 ID
settings-models-row-desc = 描述
settings-models-row-context = 上下文窗口
settings-models-ph-context = token 数，如 1000000
settings-models-row-max-tokens = 最大输出 tokens
settings-models-ph-max-tokens = token 数，如 131072
settings-models-row-wire-apis = Wire API
settings-models-row-agents = 启用的 Agents
settings-models-row-supports-tools = 工具调用
settings-models-row-supports-images = 图片输入
settings-models-add-model = 添加模型
settings-models-err-provider-name = 第 {$index} 个 Provider：名称不能为空
settings-models-err-endpoint-url = Provider「{$name}」：端点 URL 不能为空
settings-models-err-endpoint-dup = Provider「{$name}」：端点「{$wire}」重复
settings-models-err-remote-url = Provider「{$name}」：远程模型 URL 不能为空
settings-models-err-model-id = Provider「{$name}」：模型 ID 不能为空
settings-models-err-model-dup = Provider「{$name}」：模型「{$id}」重复
settings-models-err-number = Provider「{$name}」模型「{$id}」：「{$field}」需为整数 token 数
settings-models-err-env-key = Provider「{$name}」：环境变量名不能为空

### views/settings.rs — 个性化面板
settings-section-personality = 个性
settings-row-personality = 个性
settings-desc-personality = 选择 manox 回复的默认语气
settings-value-friendly = 亲和

settings-section-memory = 记忆
settings-tag-experimental = 实验性
settings-desc-memory = 设置 manox 如何收集、保留和整合记忆
settings-row-memory-enabled = 启用记忆
settings-desc-memory-enabled = 从聊天中生成新记忆，并将其带入新聊天
settings-row-memory-skip-tool = 跳过工具辅助对话
settings-desc-memory-skip-tool = 请勿从使用了 MCP 工具或网页搜索的对话中生成记忆
settings-btn-reset = 重置
settings-row-memory-reset = 重置记忆
settings-desc-memory-reset = 删除所有 manox 记忆

### views/settings.rs — MCP 面板
### views/plugin_manager.rs
### views/settings.rs — 环境面板
settings-panel-environment = 环境
settings-desc-environment = 本地环境用于指示 manox 如何为项目设置工作树
settings-section-projects = 选择项目
settings-btn-add-project = 添加项目
settings-tag-saas = saas
settings-tag-dspo = dspo

### workspace.rs
message-list-jump-to-latest = 回到底部
slash-plan-desc = 切换 plan 模式（只读调研、plan 文件、结构化批准）；`/plan <提示>` 进入 plan 模式并开始规划该提示
workspace-input-placeholder = 输入消息，点击发送以开始使用
workspace-composer-placeholder = 编写 markdown…（Cmd-Enter 发送）
workspace-unknown-command = 未知命令：/{$name}（用 `/` 菜单查看已安装命令）
workspace-unknown-skill = 未知技能：/{$name}（用 `/` 菜单查看已安装技能）
workspace-no-model = 未配置模型
workspace-approval-title = 工具调用审批
workspace-escalation-allow-once = 允许一次
workspace-escalation-allow-once-desc = 仅本次允许该调用。
workspace-escalation-always-allow = 始终允许
workspace-escalation-always-allow-desc = 本次会话内始终允许该工具。
workspace-escalation-deny = 拒绝
workspace-escalation-deny-desc = 拒绝执行；原因将返回给模型。
workspace-escalation-no-verdict-reason = 安全审查器未对此次调用运行。
workspace-clarify-title = 澄清问题
workspace-ask-supplement-label = 补充说明
workspace-ask-supplement-placeholder = 添加可选补充说明
workspace-ask-recommended = 推荐
workspace-cancel = 取消
workspace-mode-autopilot-title = 自动驾驶
workspace-mode-autopilot-desc = 安全审查器自动批准安全工具调用，风险操作将被拒绝
workspace-mode-danger-title = 危险驾驶
workspace-mode-danger-desc = 工具调用免审批，bash 在沙箱外运行
workspace-chip-mode-autopilot = 自动驾驶
workspace-chip-mode-danger = 危险驾驶
workspace-mode-title = 如何批准 manox 操作？
workspace-mode-learn-more = 了解更多
workspace-mode-notice = { $mode ->
    [autopilot] 自动驾驶模式：安全工具调用免提示，风险操作将被拒绝。
   *[danger] 危险驾驶：工具调用免审批，bash 在沙箱外运行。
}
workspace-approval-autopilot-allowed = ✔ 自动批准：{$tool}
workspace-approval-autopilot-escalated = ✗ 已升级待裁决：{$tool} — {$reason}
workspace-project-choose = 选择项目
workspace-project-new = 新建项目
workspace-project-blank = 新建空白项目
workspace-project-select-folder = 选择文件夹
workspace-project-name-prompt = 项目文件夹名称
workspace-empty-prompt = 我们该做什么？
workspace-loading-history = 正在加载对话…
### views/composer_menu.rs
composer-add-label = 添加
composer-plugins-label = 插件
composer-add-files = 文件和文件夹
composer-attach-editor = 附加编辑器
composer-goal-name = 目标
composer-goal-desc = 设置持续努力实现的目标
completion-tag-command = 命令
completion-tag-skill = 技能

### 用户消息导航
turn-navigator-search-placeholder = 搜索用户消息…
turn-navigator-empty = 暂无用户消息
turn-navigator-no-results = 没有匹配的消息
turn-navigator-attachment-only = 仅附件消息
turn-navigator-empty-message = 空消息
turn-navigator-copied = 消息已复制到剪贴板。

### slash_command.rs
slash-compact-desc = 压缩对话：把较早的历史摘要成一份交接说明，让会话越过上下文上限继续进行
slash-danger-desc = 切换到危险驾驶（免审批 + bash 沙箱外）；带提示词则切换后直接开工
slash-exit-desc = 归档当前会话并开始一个新会话
slash-new-desc = 归档当前会话并开始新会话，保留项目、驾驶模式与模型
### main.rs (system menus)
menu-settings = Settings…
menu-quit = 退出
menu-open-manox = 打开 Manox
menu-file = File
menu-about = 关于 Manox
menu-tools = 工具
menu-vscode-open = 打开 VS Code

## ChatGPT.app 启动通知（工具 → ChatGPT.app 菜单级联）
chatgpt-app-launched = 已启动 ChatGPT.app · { $provider } · { $model }
chatgpt-app-launch-failed = 启动 ChatGPT.app 失败

## VS Code 启动通知（工具 → VS Code 单一入口，注入按 vscode_app 设置解析）
vscode-app-launched = 已启动 VS Code
vscode-app-launch-failed = 启动 VS Code 失败

### terminal-ui (overlay status / search)
terminal-starting = 正在启动…
terminal-search-status = 搜索：{ $pattern }（{ $count } 处匹配）

### views/title_menu.rs
titlebar-pin = 置顶会话
titlebar-unpin = 取消置顶
titlebar-archive = 归档对话
titlebar-unarchive = 取消归档
titlebar-sidebar-toggle = 打开侧边聊天
titlebar-copy-label = 复制
titlebar-copy-id = 复制会话 ID
titlebar-copy-markdown = 复制为 Markdown
titlebar-copy-cwd = 复制工作目录
titlebar-copy-deeplink = 复制深度链接
titlebar-branch-label = 分支
titlebar-branch-from-here = 从当前消息分支
titlebar-branch-from-start = 从对话起点分支
titlebar-schedule = 添加计划任务...
titlebar-new-window = 在新窗口中打开
# ── 环境信息面板 ──────────────────────────────────────────────────────
workspace-env-no-project = 暂无项目
workspace-env-usage = 消费
workspace-env-sources = 来源
workspace-env-no-sources = 暂无来源
workspace-env-git-unavailable = git 不可用
workspace-env-git-not-a-repo = 非 git 仓库
workspace-env-git-detached = 分离头指针
workspace-env-git-copied-branch = 已复制分支名到剪贴板。
workspace-env-git-copied-worktree-name = 已复制工作区名到剪贴板。
workspace-env-git-copied-worktree-path = 已复制工作区路径到剪贴板。

# ── 上下文栏（右侧边栏）────────────────────────────────────────────────
context-rail-title = 对话信息
context-tooltip-main-calls = 主调用
context-tooltip-side-calls = 辅助调用
context-tooltip-calls-unit = 次
# ── Cockpit（运行状态 / 里程碑 / 上下文预算）──────────────────────────
# 运行状态行的阶段标签（三状态 tag：生成中 / 思考中 / 待输入）。
# "待输入"标签归并 idle / stopped / failed / awaiting approval。
# 计划区段标题。
cockpit-milestones-header = 计划
# 计划进度计数，显示在标题栏右侧。{$done}/{$total} 为已完成/总数。
cockpit-plan-progress = {$done}/{$total}
# 折叠态下当前任务之外的剩余任务数。{$count} 为数字。
cockpit-plan-remaining = +{$count} 项待办
# 折叠态下全部任务完成的提示。
cockpit-plan-all-done = 全部完成
composer-pasted-image = 粘贴的图片
composer-image-process-failed = 部分粘贴的图片无法发送（格式不支持或过大）
composer-placeholder-followup = 要求后续变更…
queued-steer-action = 引导
queued-steer-retry-action = 重试引导
queued-delete-action = 移除
queued-more-action = 更多
message-steer-pending-badge = 待引导
message-steered-badge = 已引导
# Plan review card verdict buttons
### about.rs (About window)
about-title = 关于 Manox
about-ok = 确定
about-copy = 复制

# Background task status card
background-task-kind-command = 监视器（命令）
background-task-kind-websocket = 监视器（WebSocket）
background-task-kind-bash = 后台 Bash
background-task-status-running = 运行中
background-task-status-stopping = 正在停止
background-task-status-completed = 已完成
background-task-status-failed = 失败
background-task-status-timed-out = 已超时
background-task-status-stopped = 已停止
background-task-status-session-ended = 会话已结束
background-task-stop = 停止

slash-goal-desc = 创建或管理持久目标（`/goal <目标>`、pause、resume、edit、clear）
goal-popover-title = 目标
goal-popover-objective = 目标内容
goal-popover-status = 状态
goal-popover-elapsed = 已运行
goal-popover-reason = 状态原因
goal-popover-tokens = 已用 token
goal-popover-budget = token 预算
goal-popover-remaining = 剩余 token
goal-popover-pause = 暂停
goal-popover-resume = 恢复
goal-popover-edit = 编辑
goal-popover-edit-budget = 编辑预算
goal-popover-replace = 替换
goal-popover-new = 新建目标
goal-popover-clear = 清除目标
goal-status-active = 目标进行中
goal-status-paused = 目标已暂停
goal-status-blocked = 目标受阻
goal-status-budget-limited = 目标已达预算
goal-status-complete = 目标已完成
settings-item-mcp = MCP 服务器
settings-panel-mcp = MCP 服务器
settings-desc-mcp = 连接外部工具和数据源
settings-mcp-restart-note = 开关将在下次启动时生效。
settings-section-mcp-servers = 服务器
settings-empty-mcp = 尚未配置任何 MCP 服务器。请在 mcp.toml 中添加，或安装声明了服务器的插件。
settings-btn-add-server = 添加服务器
settings-row-mcp-server-name = 服务器
settings-mcp-status-disabled = 已禁用
settings-mcp-status-not-connected = 未连接
settings-mcp-tool-count = { $count } 个工具
plugins-search-placeholder = 搜索插件…
plugins-tab-marketplace = 市场
plugins-tab-plugin = 插件
plugins-busy = 正在处理…
plugins-select = 选择
plugins-delete = 删除
plugins-update = 更新
plugins-install = 安装
plugins-uninstall = 卸载
plugins-installed = 已安装
plugins-not-installed = 未安装
plugins-enabled = 已启用
plugins-disabled = 已禁用
plugins-enable = 启用
plugins-disable = 禁用
plugins-marketplace-url = Git URL，例如 https://github.com/org/marketplace.git
plugins-add-marketplace = 添加市场
plugins-marketplace-count = {$count} 个插件
plugins-marketplace-detail = {$name} 插件
plugins-empty-marketplaces = 尚无市场。
plugins-empty-marketplace-selection = 选择一个市场来管理其中的插件。
plugins-empty-marketplace-plugins = 此市场没有插件。
plugins-empty-installed = 尚未安装插件。
plugins-error-marketplace-url = 请输入市场 Git URL。
plugins-notice-marketplace-added = 市场已添加。
plugins-notice-marketplace-updated = 市场已更新。
plugins-notice-marketplace-removed = 市场已删除。
plugins-notice-plugin-installed = 插件已安装。重启 manox 后会加载新注册的工具、技能、agent、hook 和 MCP 服务器。
plugins-notice-plugin-removed = 插件已移除。重启 manox 后会卸载启动时加载的运行时注册表。
plugins-notice-plugin-enabled = 插件已启用。重启 manox 后会加载其工具、技能、agent、hook 和 MCP 服务器。
plugins-notice-plugin-disabled = 插件已禁用。重启 manox 后会卸载启动时加载的运行时注册表。
member-running = 运行中
member-idle = 空闲
member-disbanded = 已离线
member-tasks = 任务
member-tasks-mine = 我的
member-tasks-unassigned = 未分配
member-no-tasks = 暂无任务
member-tab = { $name }
team-chip = 团队 · { $count }
team-drawer-title = 团队
team-drawer-empty = 暂无成员
team-drawer-tasks = { $count ->
    [one] { $count } 个任务
   *[other] { $count } 个任务
}