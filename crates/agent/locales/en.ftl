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
sidebar-copy-thread-id = Copy thread id
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
member-running = running
member-idle = idle
member-disbanded = gone
member-tasks = Tasks
member-tasks-mine = Mine
member-tasks-unassigned = Unassigned
member-no-tasks = No tasks
member-tab = { $name }
member-editor-tab = Editor
member-close-tab = Close tab
team-chip = team · { $count }
team-drawer-title = Team
team-drawer-empty = No members
team-drawer-tasks = { $count ->
    [one] { $count } task
   *[other] { $count } tasks
}
message-user-role = You
recap-card-title = Context compacted
retry-badge = Retrying… { $attempt }/{ $max } · { $secs }s · { $reason }
message-omitted-prefix = …(earlier omitted)
status-pending = Pending approval
status-running = Running
status-success = Done
status-continued = Continued
status-error = Error
status-denied = Denied

### views/message.rs — Thinking status row
thinking-live = Thinking for { $count }s
thinking-done = Thought for { $count }s
thinking-done-label = Thought
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

### views/settings.rs — General panel
settings-panel-general = General
settings-section-work-mode = Work mode
settings-desc-work-mode = How much technical detail manox shows
settings-row-work-mode-programming = For programming
settings-desc-work-mode-programming = More technical responses and controls
settings-row-work-mode-workday = For daily work
settings-desc-work-mode-workday = Just as capable, with less technical detail

settings-section-permissions = Permissions
settings-row-permission-default = Default permissions
settings-desc-permission-default = By default, manox can read and edit files in its workspace. When needed, it can request additional access permissions
settings-row-permission-auto-review = Automatic review
settings-desc-permission-auto-review = manox can read and edit files in its workspace. manox automatically reviews additional access requests. Automatic review can make mistakes.
settings-row-permission-full = Full access permissions
settings-desc-permission-full = When manox runs with full access, it can edit any file on your computer and run internet commands without your approval. This significantly increases the risk of data loss, leaks, or unintended actions.
settings-link-learn-more = Learn more

settings-section-general-misc = General
settings-row-file-target = Default file open target
settings-desc-file-target = Where files and folders open by default
settings-row-language = Language
settings-desc-language = Application UI language
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
settings-value-auto-detect = Auto-detect
settings-value-en = English
settings-value-zh-CN = Simplified Chinese
settings-value-bottom = Bottom
settings-value-right = Right
settings-value-inline = Inline view
settings-value-detached = Detached view

settings-section-editor = Editor
settings-row-send-shortcut = Send shortcut
settings-desc-send-shortcut = Choose when Enter sends a prompt or inserts a new line
settings-value-enter-shift = ⌘ + Enter for multiline prompts
settings-row-follow-up = Follow-up behavior
settings-desc-follow-up = Queue follow-up operations while manox is running, or steer the current run. Press "⌘ + ⌥ + /" to undo the last queued message
settings-value-queue = Queue
settings-value-steer = Steer

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
settings-row-dictation-dictionary = Dictation dictionary
settings-desc-dictation-dictionary = Words and short phrases dictation should recognize
settings-row-dictation-history = Recent dictations
settings-desc-dictation-history = Your recent dictations will be shown here, so you can find content when it did not appear where expected
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
settings-btn-open = Open
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

### views/settings.rs — Personalization panel
settings-panel-personalization = Personalization
settings-section-personality = Personality
settings-row-personality = Personality
settings-desc-personality = Choose the default tone of manox's replies
settings-value-friendly = Friendly

settings-section-custom-instructions = Custom instructions
settings-desc-custom-instructions = Provide additional instructions and context for all tasks on this machine to manox
settings-input-custom-instructions = Add custom instructions…
settings-btn-save = Save
settings-btn-saved = Saved

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
settings-panel-mcp = MCP servers
settings-desc-mcp = Connect external tools and data sources
settings-empty-mcp = No MCP servers configured. Click "Add server" to register one.
settings-section-mcp-servers = Servers
settings-btn-add-server = + Add server
settings-section-mcp-plugins = From plugins
settings-row-mcp-plugin-name = manox_apps

### views/plugin_manager.rs
plugins-title = Plugins
plugins-search-placeholder = Search plugins, skills, MCP…
plugins-tab-marketplace = Marketplace
plugins-tab-plugin = Plugin
plugins-tab-skill = Skill
plugins-tab-mcp = MCP
plugins-busy = Working…
plugins-new = New
plugins-edit = Edit
plugins-view = View
plugins-copy = Copy
plugins-select = Select
plugins-delete = Delete
plugins-update = Update
plugins-install = Install
plugins-uninstall = Uninstall
plugins-installed = Installed
plugins-not-installed = Not installed
plugins-description = Description

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

plugins-skill-new = New user skill
plugins-skill-edit = Edit user skill
plugins-skill-name = Skill name
plugins-skill-body = Skill body
plugins-origin-user = User skill
plugins-origin-plugin = Plugin: {$name}
plugins-empty-skills = No skills found.
plugins-notice-skill-saved = Skill saved. Restart manox or start a new process to refresh the model-visible skill registry.
plugins-notice-skill-removed = Skill removed. Restart manox or start a new process to refresh the model-visible skill registry.

plugins-mcp-new = New MCP server
plugins-mcp-edit = Edit MCP server
plugins-mcp-name = Server name
plugins-mcp-command = Command, for example npx
plugins-mcp-args = Args, space separated
plugins-mcp-url = Streamable HTTP URL
plugins-mcp-user = User mcp.toml
plugins-mcp-plugin = Plugin-declared MCP
plugins-empty-mcp = No user MCP servers configured.
plugins-empty-plugin-mcp = No plugin-declared MCP servers found.
plugins-notice-mcp-saved = MCP server saved to mcp.toml. Restart manox to connect it.
plugins-notice-mcp-removed = MCP server removed from mcp.toml. Restart manox to disconnect a server already loaded at startup.

### views/settings.rs — Environment panel
settings-panel-environment = Environment
settings-desc-environment = Local environment for indicating how manox should set up a worktree for a project
settings-section-projects = Select a project
settings-btn-add-project = Add project
settings-row-project = {$name}
settings-tag-saas = saas
settings-tag-dspo = dspo

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
workspace-plan-continue = Continue discussion
workspace-plan-approve = Approve and execute
workspace-clarify-title = Clarifying question
workspace-clarify-other = Other (free input)
workspace-ask-prev = Previous
workspace-ask-next = Next
workspace-ask-response = Free-form response (overrides all selections)
workspace-cancel = Cancel
workspace-submit = Submit
workspace-mode-normal = Normal
workspace-mode-section = Mode
workspace-mode-on-request-title = Request approval
workspace-mode-on-request-desc = Always ask when editing external files or using the internet
workspace-mode-auto-review-title = Approve for me
workspace-mode-auto-review-desc = Only request approval for detected risky operations
workspace-mode-yolo-title = Full access
workspace-mode-yolo-desc = Unrestricted access to the internet and any file on your computer
workspace-chip-mode-on-request = Request approval
workspace-chip-mode-auto-review = Approve for me
workspace-chip-mode-yolo = Full access
workspace-mode-title = How should manox actions be approved?
workspace-mode-learn-more = Learn more
workspace-mode-notice = { $mode ->
    [on-request] Switched to request-approval mode.
    [auto-review] Approve-for-me mode: safe tool calls run without prompting, risky ones still ask.
   *[yolo] Full access: tool calls need no approval, bash runs outside the sandbox.
}
workspace-approval-auto-review-note = Auto-review: {$reason}
workspace-project-choose = Choose project
workspace-project-new = New project
workspace-project-blank = Create blank project
workspace-project-select-folder = Select folder
workspace-project-name-prompt = Project folder name
workspace-yolo-on-notice = Full access on: tool calls need no approval, bash runs outside the sandbox.
workspace-yolo-off-notice = Switched to request-approval mode: approvals and sandbox restored.
workspace-empty-prompt = What should we do?
workspace-effort-section = Reasoning effort

### views/composer_menu.rs
composer-add-label = Add
composer-plugins-label = Plugins
composer-commands-label = Commands
composer-memory-label = Memory
composer-skills-label = Skills
composer-add-files = Files and folders
composer-attach-editor = Attach editor
composer-goal-name = Goal
composer-goal-desc = Set a goal for sustained effort
composer-plan-mode-name = Plan mode
composer-plan-mode-desc = Enter plan mode
composer-generate-memory = Generate
composer-tag-personal = Personal
composer-tag-system = System
completion-tag-command = Command
completion-tag-skill = Skill
completion-tag-agent = Agent

### slash_command.rs
slash-yolo-desc = Switch to Full access (no approvals + bash outside sandbox); with a prompt, switches and starts working immediately
slash-plan-desc = Enter plan mode: read-only tools only, research then submit a plan for approval (bare `/plan` toggles; `/plan <prompt>` enters with a prompt)
slash-goal-desc = Set a completion condition the agent works toward until met (bare `/goal` shows status, `/goal <condition>` sets it, `/goal clear` stops)
slash-compact-desc = Compact the conversation: summarize older history into a handoff note so the thread can keep going past the context limit
workspace-chip-plan-mode = Plan mode
workspace-chip-goal-active = Goal active
goal-popover-title = Goal active
goal-popover-condition = Condition
goal-popover-elapsed = Elapsed
goal-popover-evaluations = Evaluations
goal-popover-last-reason = Last reason
goal-popover-clear = Clear goal

### main.rs (system menus)
menu-settings = Settings…
menu-quit = Quit
menu-file = File
menu-terminal = Terminal
menu-new-terminal = New Terminal Tab
menu-close-terminal = Close Terminal Tab

### terminal-ui (overlay status / search)
terminal-placeholder = Terminal running… type to interact
terminal-exited = Terminal exited with code { $code }
terminal-search-status = search: { $pattern }  ({ $count ->
    [one] 1 match
   *[other] { $count } matches
})

### views/title_menu.rs
titlebar-menu-label = Conversation
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
titlebar-copied-id = Conversation ID copied to clipboard.
titlebar-copied-cwd = Working directory copied to clipboard.
titlebar-copied-deeplink = Deep link copied to clipboard (manox://thread/{ $id }).
titlebar-copied-markdown = Conversation copied to clipboard as Markdown.
titlebar-pinned-notice = Conversation pinned.
titlebar-unpinned-notice = Conversation unpinned.
titlebar-archive-notice = Conversation archived.
titlebar-unarchive-notice = Conversation unarchived.
titlebar-not-implemented = Not implemented yet.

# ── Environment info panel ──────────────────────────────────────────────
workspace-env-title = Environment info
workspace-env-changes = Changes
workspace-env-no-project = No project
workspace-env-usage = Usage
workspace-env-throughput = Throughput
workspace-env-cache = Cache
workspace-env-sources = Sources
workspace-env-no-sources = No sources yet
composer-pasted-image = Pasted image
composer-image-process-failed = Some pasted images could not be sent (unsupported format or too large)
