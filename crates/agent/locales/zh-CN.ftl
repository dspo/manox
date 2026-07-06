## manox UI strings — 简体中文 (zh-CN)。
##
## 仅 UI chrome；模型面向字符串（system prompt / 工具 description / 工具错误）一律英文，不经此栈。
## 缺失键回退到 en.ftl。

### sidebar.rs
sidebar-new-chat = 新对话
sidebar-search = 搜索
sidebar-scheduled = 已安排
sidebar-plugins = 插件
sidebar-section-projects = 项目
sidebar-section-conversations = 对话
sidebar-empty-summary = (新对话)
sidebar-time-just-now = 刚刚
sidebar-time-minutes = {$count} 分钟前
sidebar-time-hours = {$count} 小时前
sidebar-time-days = {$count} 天前
sidebar-time-weeks = {$count} 周前

### message.rs
message-reasoning = 思考
message-error = 错误
message-notice = 通知
message-omitted-prefix = …（已省略前面部分）
status-pending = 待审批
status-running = 运行中
status-success = 完成
status-error = 出错
status-denied = 已拒绝

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
settings-item-chat-settings = Chat Settings
settings-search-placeholder = 搜索设置…
settings-back = 返回应用
settings-coming-soon = Coming soon…
settings-coming-soon-label = Coming soon… {$label}

### views/settings.rs — 常规面板
settings-panel-general = 常规
settings-section-work-mode = 工作模式
settings-desc-work-mode = 选择 Codex 显示多少技术细节
settings-row-work-mode-programming = 适用于编程
settings-desc-work-mode-programming = 更具技术性的回复和控制
settings-row-work-mode-workday = 适用于日常工作
settings-desc-work-mode-workday = 同样强大，技术细节更少

settings-section-permissions = 权限
settings-row-permission-default = 默认权限
settings-desc-permission-default = 默认情况下，Codex 可以读取并编辑其工作区中的文件。必要时，它可以请求额外的访问权限
settings-row-permission-auto-review = 自动审核
settings-desc-permission-auto-review = Codex 可以读取和编辑其工作区中的文件。Codex 会自动审核额外访问权限请求。自动审核可能会出错。
settings-row-permission-full = 完全访问权限
settings-desc-permission-full = 当 Codex 以完全访问权限运行时，无需你批准，即可编辑你的电脑上的任何文件并运行互联网命令。这会显著增加数据丢失、泄露或意外行为的风险。
settings-link-learn-more = 了解更多

settings-section-general-misc = 常规
settings-row-file-target = 默认文件打开目标
settings-desc-file-target = 默认打开文件和文件夹的位置
settings-row-language = 语言
settings-desc-language = 应用 UI 语言
settings-row-menu-bar = 在菜单栏中显示
settings-desc-menu-bar = 关闭窗口后，仍在 macOS 菜单栏中保留 Codex
settings-row-bottom-panel = 底部面板
settings-desc-bottom-panel = 在应用标题栏中显示底部面板控件
settings-row-terminal-location = 默认终端位置
settings-desc-terminal-location = 选择终端快捷键和环境操作在何处打开终端标签页
settings-row-keep-awake = 运行时防止休眠
settings-desc-keep-awake = 在 Codex 运行聊天时，保持电脑唤醒
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
settings-row-follow-up = 跟进行为
settings-desc-follow-up = 在 Codex 运行的同时将后续操作加入队列，或引导当前运行。按下"⌘ + ⌥ + /"可对单条消息执行撤销操作
settings-value-queue = 排队
settings-value-steer = 引导

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
settings-desc-turn-completion = 设置 Codex 完成任任务时的提醒
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
settings-desc-config-approval = 选择 Codex 何时请求批准
settings-value-on-request = 按请求
settings-row-config-sandbox = 沙盒设置
settings-desc-config-sandbox = 选择 Codex 的命令执行权限
settings-value-read-only = 只读

settings-section-workspace-deps = 工作空间依赖项
settings-row-config-version = 当前版本
settings-btn-diagnose = 🔍 诊断
settings-desc-config-diagnose = 检查当前捆绑包并记录诊断日志
settings-row-config-codex-deps = Codex 依赖项
settings-desc-config-codex-deps = 允许 Codex 安装并提供随附的 Node.js 和 Python 工具
settings-row-config-reinstall = 重置并安装工作空间
settings-desc-config-reinstall = 删除本地捆绑包，重新下载后重新加载工具
settings-btn-reinstall = 重新安装

### views/settings.rs — 个性化面板
settings-panel-personalization = 个性化
settings-section-personality = 个性
settings-row-personality = 个性
settings-desc-personality = 选择 Codex 回复的默认语气
settings-value-friendly = 亲和

settings-section-custom-instructions = 自定义指令
settings-desc-custom-instructions = 为此主机上的所有任务向 Codex 提供额外说明和上下文
settings-input-custom-instructions = 添加自定义指令…
settings-btn-save = 保存
settings-btn-saved = 已保存

settings-section-memory = 记忆
settings-tag-experimental = 实验性
settings-desc-memory = 设置 Codex 如何收集、保留和整合记忆
settings-row-memory-enabled = 启用记忆
settings-desc-memory-enabled = 从聊天中生成新记忆，并将其带入新聊天
settings-row-memory-skip-tool = 跳过工具辅助对话
settings-desc-memory-skip-tool = 请勿从使用了 MCP 工具或网页搜索的对话中生成记忆
settings-btn-reset = 重置
settings-row-memory-reset = 重置记忆
settings-desc-memory-reset = 删除所有 Codex 记忆

### views/settings.rs — MCP 面板
settings-panel-mcp = MCP 服务器
settings-desc-mcp = 连接外部工具和数据源
settings-empty-mcp = 尚未配置任何 MCP 服务器。点击"添加服务器"注册一个。
settings-section-mcp-servers = 服务器
settings-btn-add-server = + 添加服务器
settings-section-mcp-plugins = 来自插件
settings-row-mcp-plugin-name = codex_apps

### views/settings.rs — 环境面板
settings-panel-environment = 环境
settings-desc-environment = 本地环境用于指示 Codex 如何为项目设置工作树
settings-section-projects = 选择项目
settings-btn-add-project = 添加项目
settings-row-project = {$name}
settings-tag-saas = saas
settings-tag-dspo = dspo

### workspace.rs
workspace-input-placeholder = 输入消息，点击发送以开始使用
workspace-composer-placeholder = 编写 markdown…（Cmd-Enter 发送）
workspace-unknown-command = 未知命令：/{$name}（用 `/` 菜单查看已安装命令）
workspace-no-model = 未配置模型
workspace-approval-title = 工具调用审批
workspace-approval-tool = 工具：{$name}
workspace-queued = （队列中还有 {$count} 个待审批）
workspace-deny = 拒绝
workspace-always-allow = 始终允许
workspace-allow-once = 允许一次
workspace-plan-approval-title = 计划审批
workspace-plan-approval-question = 是否批准此计划？
workspace-plan-continue = 继续讨论
workspace-plan-approve = 批准并执行
workspace-clarify-title = 澄清问题
workspace-clarify-other = 其他（自由输入）
workspace-ask-prev = 上一步
workspace-ask-next = 下一步
workspace-ask-response = 自由回复（覆盖所有选项）
workspace-cancel = 取消
workspace-submit = 提交
workspace-rename-title = 重命名对话
workspace-rename-prompt = 输入新标题。留空则清除自定义名称，回退到生成的摘要。
workspace-rename-confirm = 保存
workspace-mode-normal = 普通
workspace-mode-yolo = YOLO 模式
workspace-mode-section = 模式
workspace-yolo-on-notice = YOLO 模式已开启：工具调用免审批，bash 在沙箱外运行。
workspace-yolo-off-notice = 已切换到普通模式：恢复审批与沙箱。
workspace-empty-prompt = 我们该做什么？

### views/composer_menu.rs
composer-add-label = 添加
composer-plugins-label = 插件
composer-commands-label = 命令
composer-memory-label = 记忆
composer-skills-label = 技能
composer-add-files = 文件和文件夹
composer-choose-project = 选择项目
composer-choose-project-desc = 绑定项目目录
composer-attach-zed = 附加 Zed
composer-goal-name = 目标
composer-goal-desc = 设置持续努力实现的目标
composer-plan-mode-name = 计划模式
composer-plan-mode-desc = 开启计划模式
composer-generate-memory = 生成开
composer-tag-personal = 个人
composer-tag-system = 系统

### slash_command.rs
slash-yolo-desc = 切换 YOLO 模式（免审批 + bash 沙箱外）；带提示词则开启后直接开工
slash-plan-desc = 进入计划模式：仅允许只读工具，研究后提交计划待批准（裸 `/plan` 切换；`/plan <提示>` 带提示进入）

### main.rs (system menus)
menu-settings = Settings…
menu-quit = Quit
menu-file = File
