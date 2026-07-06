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
workspace-cancel = 取消
workspace-submit = 提交
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
