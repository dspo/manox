## manox UI strings — English (fallback locale).
##
## Model-facing content (system prompt, tool descriptions, tool errors) is
## always English and never routed through these bundles. Keys are grouped by
## source file for navigability; ids use `-` (fluent forbids `.`).

### sidebar.rs
sidebar-new-chat = New chat
sidebar-search = Search
sidebar-scheduled = Scheduled
sidebar-plugins = Plugins
sidebar-section-projects = Projects
sidebar-section-conversations = Conversations
sidebar-empty-summary = (New chat)
sidebar-time-just-now = Just now
sidebar-time-minutes = { $count ->
    [one] {$count} minute ago
   *[other] {$count} minutes ago
}
sidebar-time-hours = { $count ->
    [one] {$count} hour ago
   *[other] {$count} hours ago
}
sidebar-time-days = { $count ->
    [one] {$count} day ago
   *[other] {$count} days ago
}
sidebar-time-weeks = { $count ->
    [one] {$count} week ago
   *[other] {$count} weeks ago
}

### message.rs
message-reasoning = Reasoning
message-error = Error
message-notice = Notice
message-omitted-prefix = …(earlier omitted)
status-pending = Pending approval
status-running = Running
status-success = Done
status-error = Error
status-denied = Denied

### views/settings.rs
settings-group-general = General
settings-item-general = General
settings-item-appearance = Appearance
settings-item-config = Configuration
settings-item-personalization = Personalization
settings-item-pets = Pets
settings-item-keyboard = Keyboard shortcuts
settings-group-integrations = Integrations
settings-item-snapshots = App snapshots
settings-item-mcp = MCP servers
settings-item-browser = Browser
settings-item-computer = Computer control
settings-group-coding = Coding
settings-item-hooks = Hooks
settings-item-connections = Connections
settings-item-git = Git
settings-item-environment = Environment
settings-item-worktrees = Worktrees
settings-group-archived = Archived
settings-item-archived = Archived chats
settings-item-chat-settings = Chat Settings
settings-search-placeholder = Search settings…
settings-back = Back to app
settings-coming-soon = Coming soon…
settings-coming-soon-label = Coming soon… {$label}

### workspace.rs
workspace-input-placeholder = Type a message, then send to begin
workspace-composer-placeholder = Write markdown… (Cmd-Enter to send)
workspace-unknown-command = Unknown command: /{$name} (open the `/` menu to see installed commands)
workspace-no-model = No model configured
workspace-approval-title = Tool call approval
workspace-approval-tool = Tool: {$name}
workspace-queued = ({$count} more queued for approval)
workspace-deny = Deny
workspace-always-allow = Always allow
workspace-allow-once = Allow once
workspace-plan-approval-title = Plan approval
workspace-plan-approval-prompt = The agent submitted the following plan. Approve or reject:
workspace-plan-approve = Approve and execute
workspace-clarify-title = Clarifying question
workspace-clarify-other = Other (free input)
workspace-ask-prev = Previous
workspace-ask-next = Next
workspace-ask-response = Free-form response (overrides all selections)
workspace-cancel = Cancel
workspace-submit = Submit
workspace-mode-normal = Normal
workspace-mode-yolo = YOLO mode
workspace-mode-section = Mode
workspace-yolo-on-notice = YOLO mode on: tool calls need no approval, bash runs outside the sandbox.
workspace-yolo-off-notice = Switched to normal mode: approvals and sandbox restored.
workspace-empty-prompt = What should we do?

### views/composer_menu.rs
composer-add-label = Add
composer-plugins-label = Plugins
composer-commands-label = Commands
composer-memory-label = Memory
composer-skills-label = Skills
composer-add-files = Files and folders
composer-choose-project = Choose project
composer-choose-project-desc = Bind a project directory
composer-attach-zed = Attach Zed
composer-goal-name = Goal
composer-goal-desc = Set a goal for sustained effort
composer-plan-mode-name = Plan mode
composer-plan-mode-desc = Enter plan mode
composer-generate-memory = Generate
composer-tag-personal = Personal
composer-tag-system = System

### slash_command.rs
slash-yolo-desc = Toggle YOLO mode (no approvals + bash outside sandbox); with a prompt, enables YOLO and starts working immediately
slash-plan-desc = Enter plan mode: read-only tools only, research then submit a plan for approval (bare `/plan` toggles; `/plan <prompt>` enters with a prompt)

### main.rs (system menus)
menu-settings = Settings…
menu-quit = Quit
menu-file = File
