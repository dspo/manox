## manox UI strings — 简体中文 (zh-CN)。
##
## 仅 UI chrome；模型面向字符串（system prompt / 工具 description / 工具错误）一律英文，不经此栈。
## 缺失键回退到 en.ftl。

### sidebar.rs
sidebar-new-chat = 新对话
sidebar-search = 搜索
sidebar-scheduled = 已安排
sidebar-section-projects = 项目
sidebar-section-conversations = 对话
sidebar-section-external = 外部
sidebar-new-session-label = 新建会话
sidebar-new-session-manox = Manox 线程
sidebar-close-external = 关闭会话
sidebar-archive = 归档
external-wizard-no-model = 尚无模型支持该 agent
external-session-start-failed = 启动外部 agent 失败
sidebar-empty-summary = (新对话)
sidebar-copy-thread-id = 复制 thread id
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
member-running = 运行中
member-idle = 空闲
member-disbanded = 已离线
member-tasks = 任务
member-tasks-mine = 我的
member-tasks-unassigned = 未分配
member-no-tasks = 暂无任务
member-tab = { $name }
member-editor-tab = 编辑器
browser-tab = { $url }
browser-address-placeholder = 输入网址
browser-yield-hint = 已让出控制权（例如用于登录）。完成后请点此。
browser-yield-complete = 完成
browser-read-hint = Agent 正在读取本页 —— 页面中已登录的内容将暴露给 agent。
member-close-tab = 关闭标签页
team-chip = 团队 · { $count }
team-drawer-title = 团队
team-drawer-empty = 暂无成员
team-drawer-tasks = { $count ->
    [one] { $count } 个任务
   *[other] { $count } 个任务
}
message-user-role = 你
recap-card-title = 上下文已压缩
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
agent-metrics-tools = { $count ->
    [one] {$count} 个工具
   *[other] {$count} 个工具
}
agent-metrics-tokens = {$count} tokens
agent-metrics-running-agents = { $count ->
    [one] 运行 {$count} 个 Explore agent…
   *[other] 运行 {$count} 个 Explore agent…
}
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
thinking-files-read = 读取了 { $count } 个文件
thinking-tool-calls = 调用了 { $count } 次工具
thinking-duration = { $count } 秒

### views/settings.rs
settings-group-general = 通用
settings-item-general = 常规
settings-item-appearance = 外观
settings-item-config = 配置
settings-item-personalization = 个性化
settings-item-pets = 宠物
settings-item-keyboard = 键盘快捷键
settings-group-integrations = 集成
settings-item-snapshots = 应用快照
settings-item-plugins = 插件
settings-item-mcp = MCP 服务器
settings-item-browser = 浏览器
settings-item-computer = 电脑操控
settings-group-coding = 编码
settings-item-hooks = 钩子
settings-item-connections = 连接
settings-item-git = Git
settings-item-environment = 环境
settings-item-worktrees = 工作树
settings-group-archived = 已归档
settings-item-archived = 已归档对话
settings-item-chat-settings = 聊天设置
settings-search-placeholder = 搜索设置…
settings-back = 返回应用
settings-title = 设置
settings-coming-soon = Coming soon…
settings-coming-soon-label = Coming soon… {$label}

### views/settings.rs — 常规面板
settings-panel-general = 常规
settings-section-work-mode = 工作模式
settings-desc-work-mode = 选择 manox 显示多少技术细节
settings-row-work-mode-programming = 适用于编程
settings-desc-work-mode-programming = 更具技术性的回复和控制
settings-row-work-mode-workday = 适用于日常工作
settings-desc-work-mode-workday = 同样强大，技术细节更少

settings-section-permissions = 权限
settings-row-permission-default = 默认权限
settings-desc-permission-default = 默认情况下，manox 可以读取并编辑其工作区中的文件。必要时，它可以请求额外的访问权限
settings-row-permission-auto-review = 自动审核
settings-desc-permission-auto-review = manox 可以读取和编辑其工作区中的文件。manox 会自动审核额外访问权限请求。自动审核可能会出错。
settings-row-permission-full = 完全访问权限
settings-desc-permission-full = 当 manox 以完全访问权限运行时，无需你批准，即可编辑你的电脑上的任何文件并运行互联网命令。这会显著增加数据丢失、泄露或意外行为的风险。
settings-link-learn-more = 了解更多

settings-section-general-misc = 常规
settings-row-file-target = 默认文件打开目标
settings-desc-file-target = 默认打开文件和文件夹的位置
settings-row-language = 语言
settings-desc-language = 应用 UI 语言
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
settings-value-auto-detect = 自动检测
settings-value-en = English
settings-value-zh-CN = 简体中文
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
settings-row-dictation-dictionary = 听写词典
settings-desc-dictation-dictionary = 听写应能识别的单词或短语
settings-row-dictation-history = 最近的听写记录
settings-desc-dictation-history = 你最近的听写记录会显示在这里，便于在文本没有出现在预期位置时查找内容
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
settings-btn-open = 打开
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

### views/settings.rs — 个性化面板
settings-panel-personalization = 个性化
settings-section-personality = 个性
settings-row-personality = 个性
settings-desc-personality = 选择 manox 回复的默认语气
settings-value-friendly = 亲和

settings-section-custom-instructions = 自定义指令
settings-desc-custom-instructions = 为此主机上的所有任务向 manox 提供额外说明和上下文
settings-input-custom-instructions = 添加自定义指令…
settings-btn-save = 保存
settings-btn-saved = 已保存

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
settings-panel-mcp = MCP 服务器
settings-desc-mcp = 连接外部工具和数据源
settings-empty-mcp = 尚未配置任何 MCP 服务器。点击"添加服务器"注册一个。
settings-section-mcp-servers = 服务器
settings-btn-add-server = + 添加服务器
settings-section-mcp-plugins = 来自插件
settings-row-mcp-plugin-name = manox_apps

### views/plugin_manager.rs
plugins-title = 插件
plugins-search-placeholder = 搜索插件、技能、MCP…
plugins-tab-marketplace = 市场
plugins-tab-plugin = 插件
plugins-tab-skill = 技能
plugins-tab-mcp = MCP
plugins-busy = 正在处理…
plugins-new = 新建
plugins-edit = 编辑
plugins-view = 查看
plugins-copy = 复制
plugins-select = 选择
plugins-delete = 删除
plugins-update = 更新
plugins-install = 安装
plugins-uninstall = 卸载
plugins-installed = 已安装
plugins-not-installed = 未安装
plugins-description = 描述

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

plugins-skill-new = 新建用户技能
plugins-skill-edit = 编辑用户技能
plugins-skill-name = 技能名称
plugins-skill-body = 技能正文
plugins-origin-user = 用户技能
plugins-origin-plugin = 插件：{$name}
plugins-empty-skills = 尚无技能。
plugins-notice-skill-saved = 技能已保存。重启 manox 或启动新进程后，模型可见的技能注册表会刷新。
plugins-notice-skill-removed = 技能已删除。重启 manox 或启动新进程后，模型可见的技能注册表会刷新。

plugins-mcp-new = 新建 MCP 服务器
plugins-mcp-edit = 编辑 MCP 服务器
plugins-mcp-name = 服务器名称
plugins-mcp-command = 命令，例如 npx
plugins-mcp-args = 参数，以空格分隔
plugins-mcp-url = Streamable HTTP URL
plugins-mcp-user = 用户 mcp.toml
plugins-mcp-plugin = 插件声明的 MCP
plugins-empty-mcp = 尚未配置用户 MCP 服务器。
plugins-empty-plugin-mcp = 尚未发现插件声明的 MCP 服务器。
plugins-notice-mcp-saved = MCP 服务器已保存到 mcp.toml。重启 manox 后会连接它。
plugins-notice-mcp-removed = MCP 服务器已从 mcp.toml 删除。已在启动时加载的服务器需重启 manox 后断开。

### views/settings.rs — 环境面板
settings-panel-environment = 环境
settings-desc-environment = 本地环境用于指示 manox 如何为项目设置工作树
settings-section-projects = 选择项目
settings-btn-add-project = 添加项目
settings-row-project = {$name}
settings-tag-saas = saas
settings-tag-dspo = dspo

### workspace.rs
workspace-input-placeholder = 输入消息，点击发送以开始使用
workspace-composer-placeholder = 编写 markdown…（Cmd-Enter 发送）
workspace-unknown-command = 未知命令：/{$name}（用 `/` 菜单查看已安装命令）
workspace-unknown-skill = 未知技能：/{$name}（用 `/` 菜单查看已安装技能）
workspace-no-model = 未配置模型
workspace-approval-title = 工具调用审批
workspace-approval-tool = 工具：{$name}
workspace-queued = （队列中还有 {$count} 个待审批）
workspace-deny = 拒绝
workspace-always-allow = 始终允许
workspace-allow-once = 允许一次
workspace-inbound-title = 内置浏览器请求操作 Manox
workspace-inbound-intent = 请求：{$intent}
workspace-inbound-note = 该请求恒为确认，不受审批模式影响 —— 网页不得在未确认时驱动 agent。
workspace-inbound-allow = 允许
workspace-inbound-deny = 拒绝
workspace-clarify-title = 澄清问题
workspace-ask-supplement-label = 补充说明
workspace-ask-supplement-placeholder = 添加可选补充说明
workspace-ask-recommended = 推荐
workspace-cancel = 取消
workspace-submit = 提交
workspace-mode-normal = 普通
workspace-mode-section = 模式
workspace-mode-on-request-title = 请求审批
workspace-mode-on-request-desc = 编辑外部文件或使用网络时总是询问
workspace-mode-auto-review-title = 替我审批
workspace-mode-auto-review-desc = 仅对检测到的风险操作请求审批
workspace-mode-yolo-title = 完全访问
workspace-mode-yolo-desc = 不受限制地访问互联网和电脑上的任何文件
workspace-chip-mode-on-request = 请求审批
workspace-chip-mode-auto-review = 替我审批
workspace-chip-mode-yolo = 完全访问
workspace-mode-title = 如何批准 manox 操作？
workspace-mode-learn-more = 了解更多
workspace-mode-notice = { $mode ->
    [on-request] 已切换到请求审批模式。
    [auto-review] 替我审批模式：安全工具调用免提示，风险操作仍会询问。
   *[yolo] 完全访问：工具调用免审批，bash 在沙箱外运行。
}
workspace-approval-auto-review-note = 自动审核：{$reason}
workspace-project-choose = 选择项目
workspace-project-new = 新建项目
workspace-project-blank = 新建空白项目
workspace-project-select-folder = 选择文件夹
workspace-project-name-prompt = 项目文件夹名称
workspace-yolo-on-notice = 完全访问已开启：工具调用免审批，bash 在沙箱外运行。
workspace-yolo-off-notice = 已切换到请求审批模式：恢复审批与沙箱。
workspace-empty-prompt = 我们该做什么？
workspace-effort-section = 推理强度
workspace-provider-reload-failed = 重新加载 provider 配置失败，已保留原有 providers：{$error}

### views/composer_menu.rs
composer-add-label = 添加
composer-plugins-label = 插件
composer-commands-label = 命令
composer-memory-label = 记忆
composer-skills-label = 技能
composer-add-files = 文件和文件夹
composer-attach-editor = 附加编辑器
composer-goal-name = 目标
composer-goal-desc = 设置持续努力实现的目标
composer-plan-mode-name = 协作模式
composer-plan-mode-desc = 切换 计划 ↔ 默认
composer-generate-memory = 生成开
composer-tag-personal = 个人
composer-tag-system = 系统
completion-tag-command = 命令
completion-tag-skill = 技能
completion-tag-agent = Agent

### 用户消息导航
turn-navigator-search-placeholder = 搜索用户消息…
turn-navigator-empty = 暂无用户消息
turn-navigator-no-results = 没有匹配的消息
turn-navigator-attachment-only = 仅附件消息
turn-navigator-empty-message = 空消息
turn-navigator-copied = 消息已复制到剪贴板。

### slash_command.rs
slash-yolo-desc = 切换到完全访问（免审批 + bash 沙箱外）；带提示词则切换后直接开工
slash-plan-desc = 切换协作模式（计划 ↔ 默认）；裸 `/plan` 切换，`/plan <提示>` 带提示进入计划模式
slash-goal-desc = 设置完成条件，agent 持续工作直到满足（裸 `/goal` 显示状态，`/goal <条件>` 设置，`/goal clear` 停止）
slash-compact-desc = 压缩对话：把较早的历史摘要成一份交接说明，让会话越过上下文上限继续进行
mode-chip-default = 默认
mode-chip-plan = 计划
workspace-cycle-mode = 切换协作模式
workspace-chip-goal-active = 目标进行中
goal-popover-title = 目标进行中
goal-popover-condition = 条件
goal-popover-elapsed = 已运行
goal-popover-evaluations = 评估轮数
goal-popover-last-reason = 最新评估理由
goal-popover-clear = 清除目标

### main.rs (system menus)
menu-settings = Settings…
menu-quit = Quit
menu-file = File
menu-terminal = 终端
menu-new-terminal = 新建终端标签页
menu-close-terminal = 关闭终端标签页

### terminal-ui (overlay status / search)
terminal-placeholder = 终端运行中… 输入以交互
terminal-exited = 终端已退出，退出码 { $code }
terminal-search-status = 搜索：{ $pattern }（{ $count } 处匹配）

### views/title_menu.rs
titlebar-menu-label = 对话
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
titlebar-copied-id = 会话 ID 已复制到剪贴板。
titlebar-copied-cwd = 工作目录已复制到剪贴板。
titlebar-copied-deeplink = 深度链接已复制到剪贴板（manox://thread/{ $id }）。
titlebar-copied-markdown = 会话已复制为 Markdown 到剪贴板。
titlebar-pinned-notice = 会话已置顶。
titlebar-unpinned-notice = 会话已取消置顶。
titlebar-archive-notice = 会话已归档。
titlebar-unarchive-notice = 会话已取消归档。
titlebar-not-implemented = 尚未实现。

# ── 环境信息面板 ──────────────────────────────────────────────────────
workspace-env-changes = 变更
workspace-env-no-project = 暂无项目
workspace-env-usage = 消费
workspace-env-throughput = 穿透
workspace-env-cache = 缓存
workspace-env-output = 输出
workspace-env-cache-hit-rate = 缓存 {$pct}%
workspace-env-sources = 来源
workspace-env-no-sources = 暂无来源
workspace-env-git-unavailable = git 不可用
workspace-env-git-not-a-repo = 非 git 仓库
workspace-env-git-detached = 分离头指针
workspace-env-git-copy-branch = 复制分支名
workspace-env-git-copy-path = 复制工作区路径
workspace-env-git-exit-worktree = 退出工作区

# ── 上下文栏（右侧边栏）────────────────────────────────────────────────
context-rail-title = 对话信息

# ── Cockpit（运行状态 / 里程碑 / 上下文预算）──────────────────────────
# 运行状态行的阶段标签（三状态 tag：生成中 / 思考中 / 待输入）。
cockpit-status-thinking = 思考中
cockpit-status-streaming = 生成中
# "待输入"标签归并 idle / stopped / failed / awaiting approval。
cockpit-status-awaiting-input = 待输入
# 里程碑区段标题。
cockpit-milestones-header = 计划
# 被阻塞里程碑的尾注。{$deps} 为逗号分隔的列表。
cockpit-blocked-by = 被 {$deps} 阻塞
# 折叠的已完成里程碑汇总。{$count} 为数字。
cockpit-completed-summary = +{$count} 已完成
# 上下文预算两行。{$pct} 为剩余百分比（0–100），{$used}/{$cap} 为已用/上限的预格式化计数。
cockpit-context-remaining-ctx = 剩余上下文大小 {$pct}% {$used} / {$cap}
cockpit-context-remaining-body = 剩余请求体大小 {$pct}% {$used} / {$cap}
# 里程碑区段标题末尾的隐藏/显示提示。通用——标题也可点击切换。
cockpit-hide-tasks-hint = 点击折叠
cockpit-show-tasks-hint = 点击展开

composer-pasted-image = 粘贴的图片
composer-image-process-failed = 部分粘贴的图片无法发送（格式不支持或过大）
composer-placeholder-followup = 要求后续变更…
queued-steer-action = 引导
queued-steer-retry-action = 重试引导
queued-delete-action = 移除
queued-more-action = 更多
message-steer-pending-badge = 待引导
message-steered-badge = 已引导
plan-card-title = 计划
plan-card-download = 下载计划
plan-card-copy = 复制计划
plan-card-sidebar = 在侧边栏打开

# Plan review card verdict buttons
plan-drawer-implement = 执行
plan-drawer-clear = 清空并执行
