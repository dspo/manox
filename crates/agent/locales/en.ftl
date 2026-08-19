## manox UI strings — English (fallback locale).
##
## Model-facing content (system prompt, tool descriptions, tool errors) is
## always English and never routed through these bundles. Keys are grouped by
## source file for navigability; ids use `-` (fluent forbids `.`).

### sidebar.rs
sidebar-new-chat = New chat
sidebar-section-projects = Projects
sidebar-section-conversations = Conversations
sidebar-new-session-label = New session
sidebar-new-session-manox = Manox
session-kind-terminal = Terminal
sidebar-close-external = Close session
sidebar-resume-external = Resume session
external-session-resume-failed = Failed to resume external agent
sidebar-archive = Archive
external-wizard-no-model = No model configured for this agent
external-session-start-failed = Failed to start external agent
plain-session-start-failed = Failed to start terminal session
sidebar-empty-summary = (New chat)
sidebar-copy-thread-id = Copy thread id
sidebar-pending-auth = Waiting for approval
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
message-team = Team message
member-editor-tab = Editor
browser-tab = { $url }
browser-address-placeholder = Enter URL
browser-yield-hint = Control yielded to you (e.g. to sign in). Click when done.
browser-yield-complete = Done
browser-read-hint = Agent is reading this page — its logged-in content is exposed to the agent.
message-user-role = You
recap-card-title = Context compacted
cache-miss-label = cache miss · { $tokens } tokens
retry-badge = Retrying… { $attempt }/{ $max } · { $secs }s · { $reason }
message-omitted-prefix = …(earlier omitted)
status-pending = Pending approval
status-running = Running
status-success = Done
status-continued = Continued
status-error = Error
status-denied = Denied
status-cancelled = Cancelled

### views/message.rs — Thinking status row
context-agents-title = Agents
context-agents-captain = Captain

### views/subagent_panel.rs
subagent-panel-waiting = Waiting for sub-agent activity…
subagent-panel-final-note = Live transcript is not retained across reloads; showing the sub-agent's final answer.
plan-card-title = Plan
plan-card-download = Download plan
plan-card-copy = Copy plan
plan-verdict-execute-fresh = Approve and execute
plan-verdict-execute-compact = Approve and compact context
plan-verdict-execute-keep = Approve and keep context
plan-verdict-refine = Refine plan
plan-refine-notice = Plan mode stays on — type your feedback; the model updates the plan file and proposes again.
plan-chip-label = Plan mode
plan-chip-exit-tooltip = Exit plan mode
plan-mode-on-notice = Plan mode on: the working tree is read-only; the model researches, writes the plan file, and submits it for your approval via ProposePlan.
plan-mode-off-notice = Plan mode off: full write access restored.
thinking-tool-result = tool result
thinking-reading = { $count ->
    [one] reading {$count} file
   *[other] reading {$count} files
}
thinking-writing = { $count ->
    [one] writing {$count} file
   *[other] writing {$count} files
}
thinking-editing = { $count ->
    [one] editing {$count} file
   *[other] editing {$count} files
}
thinking-running = { $count ->
    [one] running {$count} shell command
   *[other] running {$count} shell commands
}
thinking-fetching = { $count ->
    [one] fetching {$count} page
   *[other] fetching {$count} pages
}
thinking-browsing = { $count ->
    [one] browsing {$count} action
   *[other] browsing {$count} actions
}
thinking-searching = { $count ->
    [one] searching {$count} pattern
   *[other] searching {$count} patterns
}
thinking-globbing = { $count ->
    [one] matching {$count} glob
   *[other] matching {$count} globs
}
thinking-listing = { $count ->
    [one] listing {$count} directory
   *[other] listing {$count} directories
}
thinking-other = { $count ->
    [one] {$count} other tool
   *[other] {$count} other tools
}
thinking-rounds = { $count ->
    [one] thought {$count} round
   *[other] thought {$count} rounds
}
thinking-tool-calls = { $count ->
    [one] ran {$count} tool call
   *[other] ran {$count} tool calls
}
thinking-duration = { $count }s

### views/settings.rs
settings-group-general = General
settings-item-general = General
settings-item-appearance = Appearance
settings-item-config = Configuration
settings-item-models = Models
settings-item-personalization = Personalization
settings-item-pets = Pets
settings-item-keyboard = Keyboard shortcuts
settings-group-integrations = Integrations
settings-item-snapshots = App snapshots
settings-item-plugins = Plugins
settings-item-browser = Browser
settings-item-computer = Computer control
settings-group-coding = Coding
settings-item-hooks = Hooks
settings-item-connections = Connections
settings-item-git = Git
settings-item-environment = Environment
settings-item-worktrees = Worktrees
settings-group-external-tools = External Tools
settings-item-chatgpt-app = ChatGPT.app
settings-item-vscode-app = Visual Studio Code.app
settings-group-archived = Archived
settings-item-archived = Archived chats
settings-item-chat-settings = Chat Settings
settings-search-placeholder = Search settings…
settings-back = Back to app
settings-title = Settings
settings-coming-soon = Coming soon…
settings-coming-soon-label = Coming soon… {$label}

### views/settings.rs — General panel
settings-section-work-mode = Work mode
settings-desc-work-mode = How much technical detail manox shows
settings-row-work-mode-programming = For programming
settings-desc-work-mode-programming = More technical responses and controls
settings-row-work-mode-workday = For daily work
settings-desc-work-mode-workday = Just as capable, with less technical detail

settings-section-permissions = Permissions
settings-row-permission-autopilot = AutoPilot
settings-desc-permission-autopilot = manox can read and edit files in its workspace. A safety reviewer automatically approves additional access requests. It can make mistakes.
settings-row-permission-danger = Danger
settings-desc-permission-danger = When manox runs in Danger, it can edit any file on your computer and run internet commands without your approval. This significantly increases the risk of data loss, leaks, or unintended actions.
settings-link-learn-more = Learn more

settings-section-general-misc = General
settings-row-file-target = Default file open target
settings-desc-file-target = Where files and folders open by default
settings-row-ui-language = User interface language
settings-desc-ui-language = Interface chrome locale. Applies immediately on save.
settings-row-agent-language = Conversation language
settings-desc-agent-language = Language for new conversations. Existing conversations keep their original language.
settings-save-failed-title = Settings save failed
settings-saved = Saved

# External Tools → ChatGPT.app panel
settings-panel-chatgpt-app = ChatGPT.app
settings-desc-chatgpt-top = Configure what cx injects when launching ChatGPT.app.
settings-btn-copy = Copy
settings-btn-reveal = Reveal in Finder
settings-section-chatgpt-home = Codex Home
settings-desc-chatgpt-home = Main path managing ChatGPT.app conversations, kept separate from the official one.
settings-section-chatgpt-injection = Model Injection
settings-desc-chatgpt-injection = Determines how custom models become available inside ChatGPT.app.
settings-row-chatgpt-nickname = Nickname
settings-desc-chatgpt-nickname = When set, replaces the provider name used during injection (whatever provider is launched).
settings-chatgpt-nickname-ph = Optional
settings-row-chatgpt-injection = Injection mode
settings-value-injection-list = Model list
settings-value-injection-single = Single model
settings-desc-chatgpt-injection-risk = Injecting a model list requires the CDP mechanism to inject the list into ChatGPT.app's running process; choosing it means accepting that risk.
settings-row-chatgpt-providers = Providers & LLMs
settings-chatgpt-models-loading = Loading catalog…
settings-chatgpt-models-load-failed = Failed to load: { $error }
settings-chatgpt-models-empty = No injectable models found (needs a Responses-capable endpoint).
settings-section-chatgpt-env = Variable Injection
settings-desc-chatgpt-env = Extra variables passed when launching ChatGPT.app.
settings-chatgpt-env-key-ph = NAME
settings-chatgpt-env-value-ph = value
settings-btn-add-env = Add variable
settings-section-chatgpt-more = More Settings
settings-desc-chatgpt-more = Advanced options written into the injected config.toml.
settings-desc-chatgpt-websockets = Written into the injected [model_providers.*] section. Defaults to false (custom endpoints rarely support WebSocket streaming, so HTTP streaming is used). Set true when the endpoint supports WebSocket streaming.

# External Tools → Visual Studio Code.app panel
settings-panel-vscode-app = Visual Studio Code.app
settings-desc-vscode-top = Configure the Provider and LLM for the Claude Code and Codex extensions when launching Visual Studio Code from Manox.app.
settings-section-vscode-claude = Claude Code Extension
settings-section-vscode-codex = Codex Extension
settings-row-vscode-provider = Provider
settings-vscode-no-inject = Don't inject
settings-vscode-models-loading = Loading catalog…
settings-vscode-models-load-failed = Failed to load: { $error }
settings-vscode-models-empty-anthropic = No injectable models found (needs an Anthropic-capable endpoint).
settings-vscode-models-empty-responses = No injectable models found (needs a Responses-capable endpoint).
settings-row-menu-bar = Show in menu bar
settings-desc-menu-bar = Keep manox in the macOS menu bar after the main window closes
settings-row-bottom-panel = Bottom panel
settings-desc-bottom-panel = Show bottom panel controls in the application title bar
settings-row-terminal-location = Default terminal location
settings-desc-terminal-location = Choose where terminal shortcut and environment actions open the terminal tab
settings-row-keep-awake = Prevent sleep while running
settings-desc-keep-awake = Keep your computer awake while manox is running a chat
settings-row-code-review = Code review
settings-desc-code-review = Start /review in the current chat whenever possible, or open a dedicated review chat
settings-row-import = Import work from other AI apps
settings-desc-import = Import your settings, projects, and recent chats
settings-row-licenses = View open-source licenses
settings-desc-licenses = Third-party notices for bundled dependencies
settings-btn-import = Import
settings-btn-view = View
settings-value-vscode = VS Code
settings-value-bottom = Bottom
settings-value-right = Right
settings-value-inline = Inline view
settings-value-detached = Detached view

settings-section-editor = Editor
settings-row-send-shortcut = Send shortcut
settings-desc-send-shortcut = Choose when Enter sends a prompt or inserts a new line
settings-value-enter-shift = ⌘ + Enter for multiline prompts

settings-section-pop-up = Pop-up window
settings-row-pop-up-shortcut = Pop-up window shortcut
settings-desc-pop-up-shortcut = Set a global shortcut for the pop-up window. Leave blank to keep it disabled
settings-value-disabled = Disabled
settings-value-configured = Configured
settings-btn-set = Set
settings-row-default-no-project = Default to chatting with no project
settings-desc-default-no-project = Start a new chat without needing a project

settings-section-dictation = Dictation
settings-row-microphone = Microphone
settings-desc-microphone = Used for dictation
settings-value-system-default = System default
settings-row-press-dictate = Press to dictate shortcut
settings-desc-press-dictate = Hold at any position on the desktop to dictate at the cursor
settings-row-toggle-dictate = Toggle dictate shortcut
settings-desc-toggle-dictate = Press once at any position on the desktop to start dictating, then again to stop
settings-row-keep-dictation-bar = Keep dictation bar visible
settings-desc-keep-dictation-bar = Show a small shortcut reminder when dictation is not active
settings-value-off = Off
settings-value-on = On

settings-section-notifications = Notifications
settings-row-turn-completion = Turn completion notifications
settings-desc-turn-completion = Configure when manox notifies you that a task is complete
settings-value-focus-only = Only when app loses focus
settings-row-permission-notify = Enable permission notifications
settings-desc-permission-notify = Show a notification when a permission is needed
settings-row-question-notify = Enable question notifications
settings-desc-question-notify = Show a notification when input is required to continue

### views/settings.rs — Config panel
settings-panel-config = Configuration
settings-desc-config-top = Configure approval policies and sandbox settings
settings-section-config-toml = Custom config.toml settings
settings-row-config-user = User config
settings-link-open-config = Open config.toml
settings-row-config-approval = Approval policy
settings-desc-config-approval = Choose when manox asks for approval
settings-value-on-request = On request
settings-row-config-sandbox = Sandbox settings
settings-desc-config-sandbox = Choose what command execution permissions manox has
settings-value-read-only = Read-only

settings-section-workspace-deps = Workspace dependencies
settings-row-config-version = Current version
settings-btn-diagnose = 🔍 Diagnose
settings-desc-config-diagnose = Check the current bundle and record diagnostic logs
settings-row-config-builtin-deps = Built-in dependencies
settings-desc-config-builtin-deps = Allow manox to install and provide the bundled Node.js and Python tools
settings-row-config-reinstall = Reset and reinstall workspace
settings-desc-config-reinstall = Remove the local bundle, redownload, and reload the tools
settings-btn-reinstall = Reinstall

### views/settings.rs — Models panel
settings-models-add-provider = Add provider
settings-models-no-path = Provider config path is unavailable
settings-models-reload-failed-title = Provider reload failed
settings-models-empty = No providers configured yet
settings-models-unnamed = Unnamed provider
settings-models-no-selection = Select a provider on the left to view its models
settings-models-load-error-title = Config load failed
settings-models-load-error-hint = Autosave is disabled so the unrecognized file is not overwritten. Fix the file and reopen this panel.
settings-models-ph-name = Provider display name
settings-models-section-basic = Basic info
settings-models-row-apikey = API Key
settings-models-apikey-literal = Literal
settings-models-apikey-env = Environment variable
settings-models-apikey-keychain = Keychain
settings-models-apikey-shell = Shell command
settings-models-section-env = Environment variables
settings-models-section-endpoints = Endpoint config
settings-models-add-endpoint = Add endpoint
settings-models-row-url = URL
settings-models-ph-url = https://api.example.com
settings-models-agents-all-hint = None selected = all agents
settings-models-agents-add = Add
settings-models-row-copilot = GitHub Copilot auth scheme
settings-models-env-empty = No environment variables yet
settings-models-empty-models = No models yet
settings-models-value-unset = Unset
settings-models-section-models = Model list
settings-models-mode-inline = Manual
settings-models-mode-remote = Auto
settings-models-ph-remote-url = https://api.example.com/v1/models
settings-models-row-model-id = Model ID
settings-models-row-desc = Description
settings-models-row-context = Context window
settings-models-ph-context = Tokens, e.g. 1000000
settings-models-row-max-tokens = Max output tokens
settings-models-ph-max-tokens = Tokens, e.g. 131072
settings-models-row-wire-apis = Wire API
# intentionally non-translated: the user asked to keep the zh string verbatim.
settings-models-row-agents = 启用的 Agents
settings-models-row-supports-tools = Tool calling
settings-models-row-supports-images = Image input
settings-models-add-model = Add model
settings-models-err-provider-name = Provider #{$index}: name is required
settings-models-err-endpoint-url = Provider "{$name}": endpoint URL is required
settings-models-err-endpoint-dup = Provider "{$name}": duplicate endpoint "{$wire}"
settings-models-err-remote-url = Provider "{$name}": remote models URL is required
settings-models-err-model-id = Provider "{$name}": model ID is required
settings-models-err-model-dup = Provider "{$name}": duplicate model "{$id}"
settings-models-err-number = Provider "{$name}", model "{$id}": "{$field}" must be a whole number of tokens
settings-models-err-env-key = Provider "{$name}": environment variable name is required

### views/settings.rs — Personalization panel
settings-section-personality = Personality
settings-row-personality = Personality
settings-desc-personality = Choose the default tone of manox's replies
settings-value-friendly = Friendly

settings-section-memory = Memory
settings-tag-experimental = Experimental
settings-desc-memory = Configure how manox collects, retains, and consolidates memory
settings-row-memory-enabled = Enable memory
settings-desc-memory-enabled = Generate new memories from chats and bring them into new chats
settings-row-memory-skip-tool = Skip tool-assisted conversations
settings-desc-memory-skip-tool = Do not generate memories from conversations that used MCP tools or web search
settings-btn-reset = Reset
settings-row-memory-reset = Reset memory
settings-desc-memory-reset = Delete all manox memories

### views/settings.rs — MCP panel
### views/plugin_manager.rs
### views/settings.rs — Environment panel
settings-panel-environment = Environment
settings-desc-environment = Local environment for indicating how manox should set up a worktree for a project
settings-section-projects = Select a project
settings-btn-add-project = Add project
settings-tag-saas = saas
settings-tag-dspo = dspo

### workspace.rs
slash-plan-desc = Toggle plan mode (read-only research, plan file, structured approval); `/plan <prompt>` enters plan mode and starts planning the prompt
workspace-input-placeholder = Type a message, then send to begin
workspace-composer-placeholder = Write markdown… (Cmd-Enter to send)
workspace-unknown-command = Unknown command: /{$name} (open the `/` menu to see installed commands)
workspace-unknown-skill = Unknown skill: /{$name} (open the `/` menu to see installed skills)
workspace-no-model = No model configured
workspace-reasoning-effort = Reasoning effort
workspace-reasoning-high = High
workspace-reasoning-max = Max
workspace-approval-title = Tool call approval
workspace-escalation-allow-once = Allow once
workspace-escalation-allow-once-desc = Run this call once.
workspace-escalation-always-allow = Always allow
workspace-escalation-always-allow-desc = Allow this tool for the rest of the session.
workspace-escalation-deny = Deny
workspace-escalation-deny-desc = Refuse; the reason is returned to the model.
workspace-escalation-no-verdict-reason = The safety reviewer did not run for this call.
workspace-clarify-title = Clarifying question
workspace-ask-supplement-label = Supplemental note
workspace-ask-supplement-placeholder = Add optional context
workspace-ask-recommended = Recommended
workspace-cancel = Cancel
workspace-mode-autopilot-title = AutoPilot
workspace-mode-autopilot-desc = A safety reviewer automatically approves safe tool calls; risky ones are denied
workspace-mode-danger-title = Danger
workspace-mode-danger-desc = Tool calls need no approval, bash runs outside the sandbox
workspace-chip-mode-autopilot = AutoPilot
workspace-chip-mode-danger = Danger
workspace-mode-title = How should manox actions be approved?
workspace-mode-learn-more = Learn more
workspace-mode-notice = { $mode ->
    [autopilot] AutoPilot mode: safe tool calls run without prompting, risky ones are denied.
   *[danger] Danger: tool calls need no approval, bash runs outside the sandbox.
}
workspace-approval-autopilot-escalated = ✗ Escalated for your review: {$tool} — {$reason}
workspace-project-choose = Choose project
workspace-project-new = New project
workspace-project-blank = Create blank project
workspace-project-select-folder = Select folder
workspace-project-name-prompt = Project folder name
workspace-empty-prompt = What should we do?
workspace-loading-history = Loading conversation…
### views/composer_menu.rs
composer-add-label = Add
composer-plugins-label = Plugins
composer-add-files = Files and folders
composer-attach-editor = Attach editor
composer-goal-name = Goal
composer-goal-desc = Set a goal for sustained effort
completion-tag-command = Command
completion-tag-skill = Skill

### User turn navigator
turn-navigator-search-placeholder = Search user messages…
turn-navigator-empty = No user messages
turn-navigator-no-results = No matching messages
turn-navigator-attachment-only = Attachment-only message
turn-navigator-empty-message = Empty message
turn-navigator-copied = Message copied to clipboard.

### slash_command.rs
slash-danger-desc = Switch to Danger (no approvals + bash outside sandbox); with a prompt, switches and starts working immediately
slash-exit-desc = Archive the current thread and start a fresh one
slash-new-desc = Archive the current thread and start a fresh one that keeps the project, approval mode, and model
slash-compact-desc = Compact the conversation: summarize older history into a handoff note so the thread can keep going past the context limit
### main.rs (system menus)
menu-settings = Settings…
menu-quit = Quit
menu-open-manox = Open Manox
menu-file = File
menu-about = About Manox
menu-tools = Tools
menu-vscode-open = Open VS Code

## ChatGPT.app launch notifications (Tools → ChatGPT.app menu cascade)
chatgpt-app-launched = Launched ChatGPT.app · { $provider } · { $model }
chatgpt-app-launch-failed = Failed to launch ChatGPT.app

## VS Code launch notifications (Tools → VS Code menu cascade)
vscode-app-launched = Launched VS Code
vscode-app-launch-failed = Failed to launch VS Code

### terminal-ui (overlay status / search)
terminal-starting = Starting…
terminal-search-status = search: { $pattern }  ({ $count ->
    [one] 1 match
   *[other] { $count } matches
})

### views/title_menu.rs
titlebar-pin = Pin conversation
titlebar-unpin = Unpin conversation
titlebar-archive = Archive conversation
titlebar-unarchive = Unarchive conversation
titlebar-sidebar-toggle = Open side chat
titlebar-copy-label = Copy
titlebar-copy-id = Copy conversation ID
titlebar-copy-markdown = Copy as Markdown
titlebar-copy-cwd = Copy working directory
titlebar-copy-deeplink = Copy deep link
titlebar-branch-label = Branch
titlebar-branch-from-here = Branch from here
titlebar-branch-from-start = Branch from start
titlebar-schedule = Add scheduled task...
titlebar-new-window = Open in new window
# ── Environment info panel ──────────────────────────────────────────────
workspace-env-no-project = No project
workspace-env-usage = Usage
workspace-env-sources = Sources
workspace-env-no-sources = No sources yet
workspace-env-git-unavailable = git unavailable
workspace-env-git-not-a-repo = Not a git repo
workspace-env-git-detached = detached
workspace-env-git-copied-branch = Branch name copied to clipboard.
workspace-env-git-copied-worktree-name = Worktree name copied to clipboard.
workspace-env-git-copied-worktree-path = Worktree path copied to clipboard.

# ── Context rail (right sidecar) ────────────────────────────────────────
context-rail-title = Conversation Info
context-tooltip-main-calls = Main calls
context-tooltip-side-calls = Side calls
context-tooltip-calls-unit = calls
# ── Cockpit (run status / milestones / context budget) ──────────────────
# Phase labels for the run-status row (three-tag pill: generating / reasoning /
# user-turn).
# The "user-turn" tag label (collapsed state of idle/stopped/failed/
# awaiting-approval).
# Plan section header.
cockpit-milestones-header = Plan
# Plan progress count shown at the right of the header. {$done}/{$total} are
# completed/total step counts.
cockpit-plan-progress = {$done}/{$total}
# Remaining tasks beyond the current one, shown when collapsed. {$count} is a
# number.
cockpit-plan-remaining = +{$count} to do
# Collapsed-state note when every step is completed.
cockpit-plan-all-done = All done
composer-pasted-image = Pasted image
composer-image-process-failed = Some pasted images could not be sent (unsupported format or too large)
composer-placeholder-followup = Request a follow-up change…
queued-steer-action = Steer
queued-steer-retry-action = Retry steer
queued-delete-action = Remove
queued-more-action = More
message-steer-pending-badge = Waiting to steer
message-steered-badge = Steered
# Plan review card verdict buttons
### about.rs (About window)
about-title = About Manox
about-ok = OK
about-copy = Copy

# Background task status card
background-task-kind-command = Monitor (command)
background-task-kind-websocket = Monitor (WebSocket)
background-task-kind-bash = Background Bash
background-task-kind-sailor = Sailor (subagent)
background-task-status-running = Running
background-task-status-stopping = Stopping
background-task-status-completed = Completed
background-task-status-failed = Failed
background-task-status-timed-out = Timed out
background-task-status-stopped = Stopped
background-task-status-session-ended = Session ended
background-task-stop = Stop

slash-goal-desc = Create or manage a persistent Goal (`/goal <objective>`, pause, resume, edit, clear)
goal-popover-title = Goal
goal-popover-objective = Objective
goal-popover-status = Status
goal-popover-elapsed = Elapsed
goal-popover-reason = Reason
goal-popover-tokens = Tokens used
goal-popover-budget = Token budget
goal-popover-remaining = Remaining
goal-popover-rounds = Rounds
goal-popover-pause = Pause
goal-popover-resume = Resume
goal-popover-edit = Edit
goal-popover-edit-budget = Edit budget
goal-popover-edit-rounds = Edit rounds
goal-popover-replace = Replace
goal-popover-new = New Goal
goal-popover-clear = Clear goal
goal-status-active = Goal active
goal-status-paused = Goal paused
goal-status-blocked = Goal blocked
goal-status-budget-limited = Goal budget limited
goal-status-complete = Goal complete
settings-item-mcp = MCP servers
settings-panel-mcp = MCP servers
settings-desc-mcp = Connect external tools and data sources
settings-mcp-restart-note = Switches apply from the next launch.
settings-section-mcp-servers = Servers
settings-empty-mcp = No MCP servers configured. Add one in mcp.toml or install a plugin that declares servers.
settings-btn-add-server = Add server
settings-row-mcp-server-name = Server
settings-mcp-status-disabled = Disabled
settings-mcp-status-not-connected = Not connected
settings-mcp-tool-count = { $count } tools
plugins-search-placeholder = Search plugins…
plugins-tab-marketplace = Marketplace
plugins-tab-plugin = Plugin
plugins-busy = Working…
plugins-select = Select
plugins-delete = Delete
plugins-update = Update
plugins-install = Install
plugins-uninstall = Uninstall
plugins-installed = Installed
plugins-not-installed = Not installed
plugins-enabled = Enabled
plugins-disabled = Disabled
plugins-enable = Enable
plugins-disable = Disable
plugins-marketplace-url = Git URL, for example https://github.com/org/marketplace.git
plugins-add-marketplace = Add marketplace
plugins-marketplace-count = {$count} plugins
plugins-marketplace-detail = {$name} plugins
plugins-empty-marketplaces = No marketplaces found.
plugins-empty-marketplace-selection = Select a marketplace to manage its plugins.
plugins-empty-marketplace-plugins = This marketplace has no plugins.
plugins-empty-installed = No installed plugins.
plugins-error-marketplace-url = Enter a marketplace Git URL.
plugins-notice-marketplace-added = Marketplace added.
plugins-notice-marketplace-updated = Marketplace updated.
plugins-notice-marketplace-removed = Marketplace removed.
plugins-notice-plugin-installed = Plugin installed. Restart manox to load newly registered tools, skills, agents, hooks, and MCP servers.
plugins-notice-plugin-removed = Plugin removed. Restart manox to unload runtime registries that were loaded at startup.
plugins-notice-plugin-enabled = Plugin enabled. Restart manox to load its tools, skills, agents, hooks, and MCP servers.
plugins-notice-plugin-disabled = Plugin disabled. Restart manox to unload runtime registries loaded at startup.
member-running = running
member-idle = idle
member-disbanded = gone
member-dismiss = dismiss
member-tasks = Tasks
member-tasks-mine = Mine
member-tasks-unassigned = Unassigned
member-no-tasks = No tasks
member-tab = { $name }
team-chip = team · { $count }
team-drawer-title = Team
team-drawer-empty = No members
team-drawer-tasks = { $count ->
    [one] { $count } task
   *[other] { $count } tasks
}