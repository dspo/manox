# UI Map

Shared vocabulary for every named UI component in manox. When discussing UI, reference
component names from this file so both parties refer to the same thing.

Component names use PascalCase. The hierarchy mirrors the visual containment tree.

---

## Harness 现状（pi-only）

老 manox harness（`harness-manox` crate 与 agent-ui 的 manox 变体）已完全
删除；`crates/pi`（TS Pi 内核移植）+ `crates/pi-extensions` 是唯一 harness
核心，`agent` / `agent-ui` 是宿主接线层。本文件只描述当前 pi 路径的 UI；
引用老 manox 实现请查 git 历史或 `origin/Manox` 备份分支。

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| 会话主路径（流式文本 / thinking / 工具卡片 / cancel / steer） | ✅ | `pi_backend::session` + `adapt`；空闲 `prompt()`，运行中 `steer()` |
| 会话持久化与重启恢复 | ✅ | pi jsonl + `session_meta` sidecar |
| 转录渲染（MessageList / ToolCallCard / RetryBadge / ErrorMessage） | ✅ | 共享渲染管线 |
| 审批门控（ToolCallAuthorization / AskUserQuestion / AutoPilot reviewer） | ✅ | 宿主 `ApprovalGatedTool` wrapper；AccessChip 切 AutoPilot/Danger |
| Slash commands | ✅ 部分 | `/compact`、`/exit`(`/quit`)、`/new`(`/clear` `/archive`)、`/plan`、`/goal`；markdown/skill 适配器经共享 registry；`/danger` 随 manox 移除 |
| 模型选择器 | ✅ | pi `ProviderRegistry`，按 provider 显示名分组 |
| 项目（composer chip / 侧栏文件夹 / 绑定新会话） | ✅ | 共享 threads.db `projects` 表 |
| Sidebar / 会话列表 / 新建切换归档 / LLM 标题 | ✅ | pi `SessionRepository` + sidecar；标题双模式语义移植自 manox |
| ContextRail（usage/cost/cockpit 相位/git 状态/plan/changes/branch） | ✅ | cost 来自内核 `session_stats`（rate card 计价）；cockpit 相位随事件流驱动 |
| `/compact` + Recap 卡片 | ✅ | 内核 `HarnessEvent` compaction 事件（manual/threshold/overflow） |
| 后台线程（ctrl-b 置底 / 切换自动 park） | ✅ | `background_threads` + `attach_thread` parking |
| 外部会话（Claude Code / Codex / GitHub Copilot CLI / VS Code 注入 / 终端纯 PTY） | ✅ | 侧栏 provider→model 级联（agents）+ 终端直接入口（`SpawnPlainSession`）+ `ViewMode::ExternalSession` |
| Browser 标签 / Terminal 标签 | ✅ | webview host notify/inbound 桥；平台 terminal surface |
| TurnNavigator | ✅ | cmd-m / cmd-shift-; |
| 图片附件 | ✅ 部分 | 剪贴板粘贴 → chip → 气泡渲染已通；构造 pi prompt 时图片块仍被丢弃（text-only） |
| MCP | ✅ | 连接核心共享化 + pi AgentTool 桥（#442）；Settings → MCP servers 面板（列表/连接状态/持久开关） |
| Plus 菜单（文件 / 目标 / 插件） | ⏳ | composer plus 槽位当前为空 div |
| skill / subagent @mentions | ⏳ | pi 路径无注册表接线 |
| Sub-agent 观察 | ✅ 部分 | rail 观察行（生命周期/活动）+ Agent 工具卡片实时流式子转录（text/thinking delta、工具 ▸/✓/✗ 行）；独立钻取面板随 manox 移除，卡片体即钻取面 |
| Plan 模式 / PlanReview / PlanPreview | ⏳ | manox 流程，随 harness 移除；rail 的 plan 节（`UpdatePlan`）保留 |
| Goal | ✅ 部分 | facade+GoalBridge 共享快照、GetGoal/CreateGoal/UpdateGoal 工具、`/goal` 命令、composer chip+状态 popover；per-turn 记账/自动续跑/BudgetLimited 强制为后续项 |
| Team | ✅ 部分 | 后端完整（Team/Member/TaskList 运行时 + 8 工具 + 同伴消息路由 + 授权气泡）；roster/MemberPanel UI 为后续项 |

分层纪律：crates/pi 只做 TS Pi 对齐与扩展点；harness 能力扩展一律走
crates/pi-extensions；宿主（agent / agent-ui）只做装配与 UI。

---

## 索引

### 顶层

- [Window](#window) · [NativeMenuBar](#nativemenubar) · [Workspace](#workspace) · [WorkspaceShell](#workspaceshell) · [TerminalColumn](#terminalcolumn) · [ViewMode](#viewmode) · [ViewMode::Workspace](#viewmodeworkspace-layout) · [ViewMode::Settings](#viewmodesettings) · [ViewMode::Terminal](#viewmodeterminal) · [ViewMode::ExternalSession](#viewmodeexternalsession)

### Sidebar

- [Sidebar](#sidebar) · [SidebarScrollBody](#sidebarscrollbody) · [SidebarPinnedSectionHeader](#sidebarpinnedsectionheader) · [SidebarProjectsSection](#sidebarprojectssection) · [SidebarProjectGroup](#sidebarprojectgroup) · [SidebarConversationsSection](#sidebarconversationssection) · [SidebarNewSessionMenu](#sidebarnewsessionmenu) · [SidebarThreadItem](#sidebarhreaditem) · [SidebarDivider](#sidebardivider)

### MainColumn

- [MainColumn](#maincolumn) · [TitleBar](#titlebar) · [TitleBarThreadTitle](#titlebarthreadtitle) · [TitleBarMenuButton](#titlebarmenubutton) · [Body](#body)

### ContextRail

- [ContextRail](#contextrail) · [ContextRailPanel](#contextrailpanel) · [ContextRailCollapseBtn](#contextrailcollapsebtn) · [ContextRailChangesRow](#contextrailchangesrow) · [ContextRailBranchRow](#contextrailbranchrow) · [ContextRailBranchMenu](#contextrailbranchmenu)

### Hero / LoadingIndicator

- [Hero](#hero) · [LoadingIndicator](#loadingindicator)

### MessageArea

- [MessageArea](#messagearea) · [MessageList](#messagelist) · [MessageItem](#messageitem)

### MessageItem 变体

- [UserMessage](#usermessage) · [AssistantMessage](#assistantmessage) · [ReasoningBlock](#reasoningblock) · [ThinkingStatusRow](#thinkingstatusrow) · [ToolCallCard](#toolcallcard) · [AgentTaskCard](#agenttaskcard) · [BackgroundTaskCard](#backgroundtaskcard) · [ErrorMessage](#errormessage) · [NoticeMessage](#noticemessage) · [RecapCard](#recapcard) · [CacheMissDivider](#cachemissdivider) · [RetryBadge](#retrybadge)

### Footer / Composer

- [Footer](#footer) · [Composer](#composer) · [QueuedFollowUps](#queuedfollowups) · [ComposerDivider](#composerdivider) · [AttachmentChips](#attachmentchips) · [AttachmentChip](#attachmentchip) · [ComposerInputRow](#composerinputrow) · [InputField](#inputfield) · [SendBtn](#sendbtn) · [ModelChip](#modelchip) · [AccessChip](#accesschip) · [ProjectChip](#projectchip)

### AskDrawer

- [AskDrawer](#askdrawer) · [AskDrawerHeader](#askdrawerheader) · [AskDrawerQuestion](#askdrawerquestion) · [AskDrawerOptions](#askdraweroptions) · [AskDrawerOtherInput](#askdrawerotherinput) · [AskDrawerResponseInput](#askdrawerresponseinput) · [AskDrawerNav](#askdrawernav)

### Popups & Dropdowns

- [CompletionPopover](#completionpopover) · [ModelMenu](#modelmenu) · [AccessMenu](#accessmenu) · [ProjectMenu](#projectmenu) · [TitleMenu](#titlemenu)

### Overlays

- [BlankProjectOverlay](#blankprojectoverlay)

### EditorPane

- [EditorDivider](#editordivider) · [RightPane](#rightpane) · [RightTabBar](#righttabbar) · [EditorWriteTab](#editorwritetab) · [EditorPreviewTab](#editorpreviewtab) · [BrowserView](#browserview)

### ManagementShell

- [ManagementBackControl](#managementbackcontrol)

### Settings

- [SettingsView](#settingsview) · [SettingsTitleBar](#settingstitlebar) · [SettingsLeftNav](#settingsleftnav) · [SettingsSearchInput](#settingssearchinput) · [SettingsGroupList](#settingsgrouplist) · [SettingsGroup](#settingsgroup) · [SettingsItem](#settingsitem) · [SettingsRightPane](#settingsrightpane) · [SettingsPanel](#settingspanel) · [SettingsModelsPanel](#settingsmodelspanel) · [SettingsChatGptAppPanel](#settingschatgptapppanel) · [SettingsSectionCard](#settingssectioncard) · [SettingsRow](#settingsrow) · [SettingsSectionHeader](#settingssectionheader) · [SettingsHairline](#settingshairline)

### Terminal

- [TerminalView](#terminalview) · [TerminalTabBar](#terminaltabbar) · [TerminalGrid](#terminalgrid)

### Shared Primitives

- [Button](#shared-primitives) · [Input](#shared-primitives) · [PopupMenu](#shared-primitives) · [PopupMenuItem](#shared-primitives) · [TabBar](#shared-primitives) · [Tag](#shared-primitives) · [Markdown](#markdown) · [TerminalPanel](#terminalpanel) · [TurnFrame](#turnframe) · [Icon](#shared-primitives) · [BrailleSpinner](#braillespinner) · [ScrollHandle](#shared-primitives) · [TitleBar](#shared-primitives) · [ContextMenu](#shared-primitives) · [Tooltip](#shared-primitives)

### 状态

- [ApprovalMode](#approval-modes) · [ToolCallStatus](#tool-call-statuses)

---

## 1. Window

#### Window

Top-level native window, title "manox", min 900×600.

> Source: `manox/src/main.rs`

#### NativeMenuBar

macOS menu bar built by `build_app_menus()`: `manox` (About/Settings…/Quit), `Terminal` (new/close tab), and `工具` (Tools) with two app cascades. `ChatGPT.app` → provider → model: models mirror the provider registry snapshot filtered by `visible_agents()` containing `ChatGPT.app` (Responses-capable models), grouped by provider; picking a model dispatches `LaunchChatGptApp { provider, model }`, routed through the App-level action handler to `Workspace::launch_chatgpt_app`, which starts ChatGPT.app via cx's injection path on a background thread (selected model = default; the provider's full Responses catalog is injected). `VS Code` → provider → model: models filtered by `visible_agents()` containing `VS Code` (Anthropic-wire models); picking a model dispatches `LaunchVSCode { provider, model }` → `Workspace::launch_vscode_app` → `cx::launch_vscode_app`, which resolves the login-shell env, overlays Claude Code BYOK env (`ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY`/`ANTHROPIC_MODEL` + provider/model env) at highest priority, and launches VS Code with `VSCODE_CLI=1` so the extension host, the Claude Code extension's bundled CLI, and integrated terminals inherit the injected env (a running VS Code is restarted after user confirmation; no settings.json writes, API key never persisted). A trailing 「打开」item dispatches `LaunchVSCodePlain` → `cx::launch_vscode_plain` (plain `open -a`). The VS Code submenu is disabled when VS Code is not installed. Text-only — gpui native menu items carry no images. Rebuilt by `i18n::rebuild_menus` on UI-language change, after a provider-registry reload, and once when the initial background provider registration lands.

> Source: `manox/src/main.rs`

#### SystemTray

Process-lifetime system tray installed right after the first main window opens (`tray::install` — ordered after window creation because the status item creates its own `NSStatusBarWindow`, which must not become a startup death mode when window-server resources are exhausted), the lifeline for reaching manox while no window exists. Backends: macOS/Windows use `tray-icon` (native status item + menu; both platforms pump the tray's messages on the gpui main thread), Linux uses `ksni` (StatusNotifierItem over D-Bus on its own thread, no GTK involvement). Menu items: 「打开 Manox」(`menu-open-manox`) and 「退出」(`menu-quit`), labels re-resolved through the `i18n::rebuild_menus` path on UI-language change. Windows additionally opens/focuses the window on left icon click (right click pops the menu); macOS pops the menu on icon click. Event bridge: gpui exposes no cross-thread wake, so a foreground task polls every 100ms and drains the backend's event channels into `TrayCmd::Open` / `TrayCmd::Quit`. With a tray, the app runs under `QuitMode::Explicit`: closing the main window parks the process instead of quitting it — the `Workspace` entity stashed in `agent_ui::dispatch` is process-lifetime, so the foreground thread and any parked background threads keep running through the close; 「打开 Manox」(or the macOS dock icon, via `on_reopen`) re-opens the window over that same workspace, restoring conversation, drafts, and thread list. Tray install failure keeps the platform default (quit-on-last-window-close off macOS) so the app never strands invisibly.

> Source: `manox/src/tray.rs`, `manox/src/main.rs`

## 2. Workspace

#### Workspace

Root container, horizontal flex (`h_flex`), owns all sub-views.

> Source: `agent-ui/src/workspace.rs`

### 2.1 ViewMode

`Workspace` switches between four mutually exclusive full-window modes:

#### ViewMode::Workspace
Default — sidebar + conversation + composer.

#### ViewMode::Settings
Full-window settings overlay with slide-in animation.

#### ViewMode::Terminal
Full-window terminal emulator, rendered through the shared [WorkspaceShell](#workspaceshell) with a [TerminalColumn](#terminalcolumn) (TitleBar + the built-in `TerminalView`) as the main column — the sidebar divider stays draggable here.

#### ViewMode::ExternalSession
Full-window external agent CLI session (claude / codex / copilot) or a plain terminal session (the user's shell — `SessionKind::Terminal`, no cx involvement). Rendered through the shared [WorkspaceShell](#workspaceshell) with a [TerminalColumn](#terminalcolumn) showing the active `ExternalSession`'s `TerminalView` (the agent's TUI or the shell) in place of the conversation; the TitleBar shows the session's live OSC title (falling back to the kind label — "Claude Code" / "Codex" / "GitHub Copilot" / 终端). The session has no dedicated titlebar close button: it is archived the same way a thread row is — via the hover archive control on its unified [SidebarThreadItem](#sidebarthreaditem) row (or the title menu). Lives in memory only — never persisted; the sidebar row is removed both on archive (agent kinds kill through the cx `SessionHandle`; a plain terminal's `PtyHandle` tears its child tree down on drop) and when the child exits on its own (a `ChildExit` subscription on the terminal tears the session down without user action). The terminal's OSC title is mirrored into `ExternalSession.title` so the titlebar and sidebar row share one `display_title()`. If the removed session was the active one, the view falls back to the conversation pane.

---

## 3. ViewMode::Workspace Layout

Every non-Settings `ViewMode` renders through one shared shell ([WorkspaceShell](#workspaceshell)): `sidebar | SidebarDivider | mode main column`. The sidebar divider (drag-resize, double-click reset, width clamp + sync to the sidebar entity) is defined in exactly one place, so the conversation, built-in terminal, and external-session views all resize the sidebar identically — only the main column differs per mode. In the default mode the main column is the conversation column: a relative `v_flex` that holds a shared [TitleBar](#titlebar) overlay on top (spanning the whole middle column) and the conversation column underneath. The [ContextRail](#contextrail) is NOT a flex sibling column — it is an absolute overlay floating over the conversation column's top-right (`absolute().top(TITLE_BAR_HEIGHT + 16).right(16).w(ENV_CARD_WIDTH).occlude()`), content height (never full-height), with the conversation body reserving `ENV_CONTENT_INSET` right padding so the message list never hides behind the card. [EditorPane](#editordpane) opens as a third top-level column to the right of the middle column when any right-pane tab is active; while it is open the card stays hidden so the conversation reclaims its width. The card also folds away below `RAIL_NARROW_BREAK` (900px middle-column width), in which case the conversation column fills the middle column.

```
┌──────────┬──┬──────────────────────────────┐
│          │  │  MiddleColumn (v_flex.relative)│
│ Sidebar  │▌ │ ┌──────────────────────────┐  │
│          │  │ │ TitleBar (shared overlay) │  │
│          │  ├──────────────────────────┤  │
│          │  │ MainColumn (conversation) │  │
│          │  │                ┌─────────┐ │  │
│          │  │                │Context  │ │  │
│          │  │                │Rail card│ │  │
│          │  │                │(float)  │ │  │
│          │  │                └─────────┘ │  │
└──────────┴──┴──────────────────────────────┘
```

#### WorkspaceShell

The shared window shell built by `Workspace::shell_root(sidebar, main)`: an `h_flex` root with `sidebar-slot | 6px SidebarDivider | mode main column`, the mode-switching actions (`FocusConversation` / `FocusTerminal` / `NewTerminalTab` / `CloseTerminalTab`), and the sidebar drag/reset handling. Every full-window `ViewMode` routes through it — the sidebar slot is the conversation `Sidebar` for Workspace / Terminal / ExternalSession modes and the [SettingsLeftNav](#settingsleftnav) for Settings; the Workspace mode chains the conversation-only actions (settings / editor / browser / completion / archive…), the right [EditorPane](#editordpane) + [EditorDivider](#editordivider) columns, and the turn-navigator overlay onto it; the Terminal and ExternalSession modes chain only their own extras. The divider drag/double-click-reset writes one shared width (`Workspace::sidebar_width`) and syncs it to both the `Sidebar` entity and the `SettingsView`, so the Settings page resizes its sidebar exactly like the app page. Terminal-style main columns are built by `Workspace::render_terminal_column` ([TerminalColumn](#terminalcolumn)).

> Source: `agent-ui/src/workspace.rs`

#### TerminalColumn

The terminal-style main column shared by [ViewMode::Terminal](#viewmodeterminal) and [ViewMode::ExternalSession](#viewmodeexternalsession): a [TitleBar](#titlebar) (leading icon + title) over a full-bleed terminal view (`flex_1`). One shape for both, so the two terminal surfaces read as peers inside the shared shell.

> Source: `agent-ui/src/workspace.rs`

### 3.1 Sidebar

Left panel, fixed width (260px default, 200–480 draggable).

#### Sidebar

Full-height left panel, vertical flex, `bg:background`, right border.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarScrollBody

Scrollable body inside Sidebar (`overflow_y_scroll`, `.track_scroll` on a `ScrollHandle`). Its children are two fixed slots measured by the pinned header: child 0 = the project-grouped threads (`SidebarProjectsSection`, zero-height when no registered projects exist), child 1 = the loose threads + external sessions (`SidebarConversationsSection` content, without its header — the header lives in the pinned slot).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarPinnedSectionHeader

Pinned section-header slot above the scroll body (`flex_shrink_0`, `pt(top_inset)` for the traffic-light inset): the section the user is currently reading never scrolls away. Shows the Projects header while the viewport is inside the projects section, then the Conversations header (with its `+` new-session button + dropdown) once the projects content has scrolled fully past the top. The switch threshold is the measured height of scroll-body child 0 (`bounds_for_item(0)`, fallback keeps Projects pinned on the first frame before the container is laid out). When the content fits the viewport (`max_offset() == 0`) every section header shows at its natural position instead, so the Conversations header stays reachable without scrolling.

#### SidebarProjectsSection

Middle section: project-grouped threads (if any projects exist).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarProjectGroup

Collapsible folder: chevron + folder icon + project name, indented thread list.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarConversationsSection

Loose (non-project) threads + external sessions, the scroll-body child 1 slot. The section's pinned header (`SidebarPinnedSectionHeader`) carries the `+` button opening the `SidebarNewSessionMenu` popup.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarNewSessionMenu

`PopupMenu` anchored below the "Conversations" header `+` or a project folder's `+` button (the latter carries the project path, scoping the spawned session / opened directory to that project). One flat row (Manox → `NewThread` / `NewThreadWithProject`), one flat Terminal row (plain PTY session in the menu's project directory → `SpawnPlainSession(Terminal, project)`, no cascade), one `submenu_with_icon` per external agent kind (Claude Code / Codex / GitHub Copilot), and a VS Code entry. All top-level rows use the menu component's native icon slot with a monochrome brand SVG, keeping their icon and label columns aligned. Each agent submenu is a provider→model cascade built by the shared `build_model_cascade`: models from `agent::pi_providers::global()` filtered by registration metadata `agents` containing the agent id (`claude` / `codex` / `copilot` / `VS Code`), grouped by provider display name into provider submenus, each listing its models; the emitted model id is the raw cx config key. The Terminal entry skips the cascade entirely — the workspace spawns the user's shell through `spawn_plain_session` with no provider/model injection. Picking a model in a CLI-agent cascade emits `SpawnExternalSession(kind, provider, model, project)`; in the VS Code cascade it emits `LaunchVSCode(provider, model, project)` — the workspace then launches VS Code through `cx::launch_vscode_app` with Claude Code BYOK env injected, opening the project directory (workspace cwd when launched from the Conversations header). An agent with no supporting model renders a muted "no model configured" label row instead of provider submenus. The VS Code entry renders as a disabled submenu when VS Code is not installed (parity with 工具 → VS Code).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarThreadItem

Unified row projection (`SidebarThreadItem` struct: `id`/`short_id`/`title`/`updated`/`pinned`/`has_unread`/`errored`/`running`/`pending_auth`/`selected`/`indent`/`icon`/`wash`/`kind`) rendered by one `render_thread_item` for both native threads and external-agent sessions. The two kinds are merged into one recency-ordered list (loose rows under "Conversations", project-bound rows inside their folder group) and share the selection-slide wash animation, the hover/active wash, and the hover archive action. Native thread row: unread red dot (8px, `theme.info`, shown when `summary.has_unread`), pinned star, error triangle, pending-auth spinner (accent, tooltip "Waiting for approval", shown while a tool authorization awaits the user's verdict — the thread's approval card is only visible when it is the active thread), title, short-id tag (shimmer while running), relative time, hover archive button. The hover/active/selected wash uses the thread's last saved approval-mode color (`theme.info` AutoPilot / `theme.danger` Danger). The red dot marks a thread that finished a turn while the user was viewing another thread; it clears when the user switches into the thread. External rows reuse the same layout but swap the leading icon for the agent's brand SVG (`claude.svg` / `codex.svg` / `githubcopilot.svg`), carry the same visible wash as AutoPilot threads (`theme.info` — `theme.accent` resolves to the near-white `neutral-100` in the forced Light theme, invisible on the sidebar), show the cx session id prefix in the short-id tag (click-to-copy of the full id / socket path, traceable to `~/.config/cx/sessions/<id>.sock`), and drop the unread/pin/error/running-shimmer affordances. The row kind (`RowKind::Thread { archived }` vs `RowKind::External`) routes the open click and the hover archive button to the right `SidebarEvent` (`OpenThread` / `ArchiveThread` vs `OpenExternalSession` / `ArchiveExternalSession` — the latter kills + drops the `SessionHandle`, the unified archive semantics).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarDivider

6px drag handle between Sidebar and the mode main column, `cursor:col-resize`. Constructed once inside [WorkspaceShell](#workspaceshell), so it appears — and behaves identically (drag-resize, double-click reset to the 260px default) — in the conversation, terminal, and external-session views.

> Source: `agent-ui/src/workspace.rs`

### 3.2 MainColumn

Central conversation column, flex-1. The sole flex child of the middle column under the shared [TitleBar](#titlebar) overlay that spans the whole middle column (not a top-level column itself). The [ContextRail](#contextrail) floats over this column's top-right as an absolute overlay (not a flex sibling); the conversation body reserves `ENV_CONTENT_INSET` right padding when the card is shown so the message list clears it. The composer no longer spans underneath the card.

#### MainColumn

Vertical flex container, fills remaining width.

> Source: `agent-ui/src/workspace.rs`

#### TitleBar

Absolute-positioned top bar at the middle-column level (not the conversation column), height `TITLE_BAR_HEIGHT`, spans both [MainColumn](#maincolumn) and the [ContextRail](#contextrail) card so the pair reads as one middle column under a single bar. Contains thread title and "..." menu.

> Source: `agent-ui/src/workspace.rs`

#### TitleBarThreadTitle

Thread title text, clickable → opens [TitleMenu](#titlemenu).

> Source: `agent-ui/src/workspace.rs`

#### TitleBarMenuButton

"..." button → opens [TitleMenu](#titlemenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### Body

Vertical flex below TitleBar, `pt:TITLE_BAR_HEIGHT`, houses [Hero](#hero) (or the [LoadingIndicator](#loadingindicator) while a session restores) or [MessageArea](#messagearea) + [Footer](#footer).

> Source: `agent-ui/src/workspace.rs`


#### 3.2.1 Hero

Shown when the thread has no substantive messages (and is not loading).

#### Hero

Vertically centered welcome area: logo/heading + inline [Composer](#composer).

> Source: `agent-ui/src/workspace.rs`

#### LoadingIndicator

Centered BrailleSpinner + "Loading conversation…" (`workspace-loading-history`), shown in place of the [Hero](#hero) while a sidebar-opened session's history is still restoring. The composer is hidden while loading — input is gated on the thread's `HistoryPhase` until `Ready`; preview batches stream into the [MessageArea](#messagearea) incrementally (`ThreadEvent::HistoryProgress`).

> Source: `agent-ui/src/workspace.rs`

#### 3.2.2 MessageArea

Shown when the thread has messages. Replaces [Hero](#hero).


#### MessageArea

Wraps [MessageList](#messagelist).

> Source: `agent-ui/src/workspace.rs`

#### MessageList

Index-anchored virtual list via the first-party `vlist` element (`vlist(list_state, render_item)`, bottom alignment, 2048px overdraw). It replaces `gpui::list` for this view: gpui's sum-tree height cache could store exploded item heights under edge-condition measurement frames (zero/min-content width), surfacing as huge blank regions between messages. `vlist` owns its height cache and enforces the invariants that prevent that class of bug: items are only measured at the list's definite pixel width (a zero-width frame measures nothing and keeps the old cache), unmeasured items carry a small constant estimate rather than a content-derived guess, and the scroll position is a logical anchor `ListOffset { item_ix, offset_in_item }` so re-measurement never shifts the visible content, and every row in the draw range is re-measured every frame (not only rows an explicit `remeasure` flagged) so a height that changed without a remeasure signal — async image load, font swap, lazy markdown reflow — self-corrects instead of painting at a stale cached height. `FollowMode::Tail` re-pins to the end each layout while following (disengages on upward scroll, re-arms at the bottom); count changes are reconciled via `splice` and in-place mutations via `remeasure_items` (which only discards an off-screen row's cached height early), both driven from the `ThreadEvent` handler's `ApplyOutcome`. Only the viewport + overdraw items render, so a long thread's first frame pays only for the visible turn.

> Source: `agent-ui/src/views/vlist.rs` (element + state), `agent-ui/src/workspace.rs` (wiring)

#### MessageItem

Single rendered conversation item, centered, max-width 760px. Each `MessageItem` renders one of the variant cards below based on `ConvItem` kind. Every kind that carries a text body — user, assistant, error, notice, team message, recap, retry detail, plan review — mounts a persistent `Entity<Markdown>` (`MessageItem::markdown`, created lazily by `ensure_markdown`) instead of rebuilding one per frame: a per-frame `Entity` resets the document's `DocSelection`/`FocusHandle` on every render and breaks drag-select + Cmd/Ctrl+C (the old `markdown_tv` fallback), while a persistent body keeps its selection state alive across frames and leaves inline links clickable.

> Source: `agent-ui/src/views/message.rs`

##### MessageItem variants

#### UserMessage

Full-width user turn block rendered inside [TurnFrame](#turnframe): `You > Time/DateTime > ModelID` metadata header, persistent selectable markdown body, copy btn (hover), and an approval-mode-colored frame captured at send time.

> Source: `agent-ui/src/views/message.rs`

#### AssistantMessage

Full-width block: role label + copy btn + markdown body (plain text while streaming).

> Source: `agent-ui/src/views/message.rs`

#### ReasoningBlock

Collapsible: chevron + "Reasoning" label + left-bordered muted body. Each reasoning round (an `ActivityEntry::Reasoning` inside a `Thinking` segment, plus the top-level `ConvItem::Reasoning`) owns a persistent `Entity<Markdown>` (`markdown` field) mounted on first sync — so drag-select + Cmd/Ctrl+C survive across frames (a per-frame `Entity` would reset the `DocSelection`/`FocusHandle` every render and break selection on reasoning text the same way it did on tool output). Italic styling propagates from the row's `Markdown::italic` toggle.

> Source: `agent-ui/src/views/message.rs`

#### ThinkingStatusRow

Folded batch of tool calls from one model response, rendered as one Claude Code–style status line. Header: braille spinner (live) or static dot (frozen) + "Thinking for Xs"/"Thought for Xs" label + aggregated action counts ("reading 2 files, running 1 shell command…") + chevron. Collapsed body shows only the most recent `⎿` entry; expanded lists every entry. Each `⎿` entry (`render_activity_entry`) is a one-line summary (status icon + tool title, mono) that expands to its full tool output via `render_tool_output`. The elapsed counter ticks every second via a gpui background timer spawned on `TurnStarted` and self-terminating on terminal `Stop`/`Error`; `frozen_secs` pins the final value so later re-renders don't inflate it. Ordinary tool calls now fold here instead of producing standalone cards.

> Source: `agent-ui/src/views/message.rs` — `render_thinking`, `render_activity_entry`, `thinking_summary`. Container state: `ConversationState` (`ConvItem::Thinking` / `ThinkingContainer`).

#### ToolCallCard

A standalone tool-call card (`render_tool_call`) for the special-case tools that don't fold into a [ThinkingStatusRow](#thinkingstatusrow) batch — today `agent` sub-agent calls and `AskUserQuestion`. A model response's other tool calls batch into the `Thinking` container; their output renders via [TerminalPanel](#terminalpanel).

Statuses: `PendingApproval` | `Running` | `Success` | `Error` | `Denied` — see [ToolCallStatus](#tool-call-statuses).

> Source: `agent-ui/src/views/message.rs`

#### AgentTaskCard

Compact, single-line sub-agent row: `[status] type · short title`. Running and pending rows use a braille-dot spinner (`BrailleSpinner`); terminal rows use check, error, or minus icons. The title is always one line with truncation and a full-title tooltip. It deliberately renders no child text, nested messages, copy control, metrics, or expansion affordance; clicking stays a no-op. Live drill-down lives on the Agent tool-call card instead: the child session's streamed text/thinking deltas and tool lifecycle lines (`▸ Tool hint` / `✓ Tool` / `✗ Tool`) append to the card's output in real time (bridged through the Agent tool's progress channel).

> Source: `agent-ui/src/views/message.rs`

#### BackgroundTaskCard

Bordered card showing a background task's kind (Monitor command / Monitor WebSocket / Background Bash), description, status badge (Running / Stopping / Completed / Failed / Timed out / Stopped / Session ended), event count, and total bytes. The title row keeps only the description's first line (a background bash description is the full command, heredoc body included) with single-line ellipsis; the complete text is shown in a hover tooltip. The detail row (failure summary or latest event) wraps in full — it is the only UI surface for a task's error text. Running tasks show a braille spinner and a Stop button that calls `background_task::stop`. Terminal tasks show a static status icon. Updated in-place by task ID via `ThreadEvent::BackgroundTaskUpdated` — the card is created when the first event snapshot arrives and never duplicated.

> Source: `agent-ui/src/views/message.rs`

#### ErrorMessage

Rounded card, `bg:danger/0.06`, red text, "Error" label + copy btn. Body is a persistent selectable `Entity<Markdown>`.

> Source: `agent-ui/src/views/message.rs`

#### NoticeMessage

Rounded card, `bg:secondary/0.15`, muted text, "Notice" label + copy btn. Body is a persistent selectable `Entity<Markdown>`.

> Source: `agent-ui/src/views/message.rs`

#### RecapCard

Collapsible compaction summary card: chevron + book icon + "Context compacted" label + copy btn. Body is the model-generated handoff summary (markdown, not localized), mounted as a persistent selectable `Entity<Markdown>`. Collapsed by default; emitted on `ThreadEvent::Compaction` and rebuilt from `MessageContent::Compaction` on thread reload.

> Source: `agent-ui/src/views/message.rs`

#### CacheMissDivider

Slim left-aligned divider rendered above an assistant turn whose request lost the prompt cache, matching oh-my-pi's `CacheInvalidationMarkerComponent`. Rendered as a 10-character rule + muted label `"cache miss · N tokens"` (tokens formatted by `format_tokens`). Emitted on `ThreadEvent::CacheInvalidation` and inserted as a `ConvItem::CacheMiss` into the conversation list.

> Source: `agent-ui/src/views/message.rs` — `render_cache_miss`. Event handler: `agent-ui/src/conversation.rs`. Enum: `agent-ui/src/conversation.rs` (`ConvItem::CacheMiss`).

#### RetryBadge

Amber badge, `bg:warning/0.12`, braille spinner + "Retry N/M (in Xs)" text. The retry detail body, when present, is a persistent selectable `Entity<Markdown>`, re-synced when a coalesced retry rewrites the item's detail in place.

> Source: `agent-ui/src/views/message.rs`

#### 3.2.3 Footer

Bottom area of MainColumn, below [MessageArea](#messagearea) (or below [Hero](#hero) on first screen). Hidden entirely while history is loading — no input until the restore lands.

#### Footer

Vertical flex, `flex_shrink_0`, `py_2`, contains [Composer](#composer) or [AskDrawer](#askdrawer).

> Source: `agent-ui/src/workspace.rs`

##### Composer

#### Composer

Centered wrapper around the input row + chips.

> Source: `agent-ui/src/workspace.rs`

#### QueuedFollowUps

Flat stack of follow-up items parked above the input while a turn is running. Every submitted follow-up starts as **Queued**: a `Redo2` queue arrow and one-line truncated summary on the left, with an explicit Steer text button, `Delete`, and `Ellipsis` on the right. Clicking Steer immediately hides the queue row and appends an optimistic user bubble to the message list with a 「待引导」 badge. When `ThreadEvent::SteerInjected { message_id }` confirms that the turn loop drained the message at a safe join point, the existing bubble becomes persistent and its badge changes to 「已引导」; no duplicate bubble is appended. If the turn is cancelled, rejected, or exits before confirmation, the optimistic bubble becomes an invisible tombstone and the item returns as a red **Failed** queue row with Retry-Steer / Remove. A late confirmation can still heal that provisional rollback. Ordinary queued messages retain submission order and coalesce into the next turn only after the current turn task has fully unwound. Queues are retained in memory per task across task switches, but are not persisted across app restarts. `⌘ + ⌥ + /` (`UndoLastQueued`) pops the tail and cancels a matching pending backend steer.

> Source: `agent-ui/src/workspace.rs` (`render_queued_follow_ups`, `steer_follow_up`, `consume_steered_follow_up`, `mark_stranded_steers_failed`); badge render in `agent-ui/src/views/message.rs` (`render_user`); drain + event in `agent/src/thread.rs` (`drain_pending_steer`, `ThreadEvent::SteerInjected`); persisted marker in `agent/src/message.rs` (`MessageUiMetadata::steered`).

#### ComposerDivider

1px horizontal border above the composer.

> Source: `agent-ui/src/workspace.rs`

#### AttachmentChips

Horizontal row of file/plugin attachment chips (conditional).

> Source: `agent-ui/src/views/composer_menu.rs`

#### AttachmentChip

Single attachment chip: icon + filename + remove btn.

> Source: `agent-ui/src/views/composer_menu.rs`

#### ComposerInputRow

Horizontal flex: [InputField](#inputfield) + [SendBtn](#sendbtn) + chips.

> Source: `agent-ui/src/workspace.rs`

#### InputField

Multi-line auto-grow text input, placeholder text.

> Source: `agent-ui/src/workspace.rs` (via `gpui_component::Input`)

#### SendBtn

Circular button, `primary` color (idle) / `danger` color (running, acts as stop).

> Source: `agent-ui/src/workspace.rs`

#### ModelChip

Dropdown chip showing current model name → [ModelMenu](#modelmenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### AccessChip

Dropdown chip showing [ApprovalMode](#approval-modes) → [AccessMenu](#accessmenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### ProjectChip

Dropdown chip showing current project → [ProjectMenu](#projectmenu) popup.

> Source: `agent-ui/src/workspace.rs`

##### AskDrawer

Replaces [Composer](#composer) when `pending_ask` is set. Every
`ThreadEvent::ToolCallAuthorization` — interactive tools, bubbled sub-agent
auth, and AutoPilot escalations (reviewer `Ask` verdicts awaiting a user
verdict) — surfaces here as the question card; the payload's options carry
the decision.

#### AskDrawer

Multi-step question navigator replacing the footer.

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerHeader

Title + stepper "N/M".

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerQuestion

Header tag + question text.

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerOptions

Checkbox/radio list with labels + descriptions.

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerOtherInput

Free-text input for "Other" option (conditional).

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerResponseInput

Free-form response input overriding all answers (conditional).

> Source: `agent-ui/src/workspace.rs`

#### AskDrawerNav

Prev / Next / Cancel / Submit buttons.

> Source: `agent-ui/src/workspace.rs`

#### 3.2.4 Popups & Dropdowns

`PopupMenu` entries are `PopupMenu` entities created on open and destroyed on close. [CompletionPopover](#completionpopover) is not a `PopupMenu` — it is a pure render overlay that never takes focus.

#### CompletionPopover

Trigger: typing `/` (slash commands) or `@` (skills + subagents) at the caret in [InputField](#inputfield). A typeahead list anchored above the composer: filters live on every keystroke, navigated with up/down, confirmed with Tab or Enter, dismissed with Escape. While open the composer wrapper sets a `completion = open` key context so the `completion == open > Input` keybindings shadow the Input's own navigation bindings. A pure render overlay — [InputField](#inputfield) keeps focus throughout, so the query keeps filtering as the user types.

> Source: `agent-ui/src/views/completion.rs` (state + detection + rendering), wired in `agent-ui/src/workspace.rs`

#### ModelMenu

Trigger: [ModelChip](#modelchip). Model selector dropdown.

> Source: `agent-ui/src/workspace.rs`

#### AccessMenu

Trigger: [AccessChip](#accesschip). [ApprovalMode](#approval-modes) selector: AutoPilot / Danger.

> Source: `agent-ui/src/workspace.rs`

#### ProjectMenu

Trigger: [ProjectChip](#projectchip). Recent projects + create blank / select folder.

> Source: `agent-ui/src/workspace.rs`

#### TitleMenu

Trigger: [TitleBarMenuButton](#titlebarmenubutton). Pin, archive, copy, schedule, new window.

> Source: `agent-ui/src/views/title_menu.rs`

#### 3.2.5 Overlays

Absolute-positioned over [Body](#body), with scrim.


#### BlankProjectOverlay

Trigger: "Create blank project" from [ProjectMenu](#projectmenu). Centered modal: project name input + confirm.

> Source: `agent-ui/src/workspace.rs`

### 3.3 ContextRail

Right-side context panel that floats over the Workspace's conversation column top-right as an absolute overlay — NOT a flex sibling of [MainColumn](#maincolumn). The `Render` impl positions it (`absolute().top(TITLE_BAR_HEIGHT + 16).right(16).w(ENV_CARD_WIDTH).occlude()`); the panel body (`render_panel`) carries the card chrome (`border_1` / `rounded(theme.radius)` / drop shadow / `bg:background` + `p_3`/`gap_2`). Content height, never full-height — a compact floating card, not a flush column or a second title bar. The conversation body reserves `ENV_CONTENT_INSET` (card width + 36px gutter) right padding so the message list never hides behind the card. Owned by `Workspace` as `Entity<ContextRail>`; the rail owns the cockpit state (run phase, the model's `PlanSnapshot`, per-cell counter animation) that used to live on `Workspace`.

Visibility is gated on the main-column body width (`ContextRail::rail_width_for`): shown as `Some(ENV_CARD_WIDTH)` (260px) at/above `RAIL_NARROW_BREAK` (900px), folded away (`None`) below it. The card's `top` clears the shared [TitleBar](#titlebar) overlay.

The card stays **hidden while the [EditorPane](#editorpane) is open** — opening the right pane reclaims the card's width for the conversation — and on the empty first screen / before the thread has interacted. It is not the editor's replacement; the editor is a third top-level column outside the middle. Because the card is absent while the editor is open, the editor-divider drag clamp reserves only `MAIN_MIN_WIDTH` (no card width) — the conversation alone holds the middle column while the editor is open. The card floats as an absolute overlay (content height); the conversation column is `flex_1`/`min_w_0` and reserves `ENV_CONTENT_INSET` right padding when the card is shown.

#### ContextRail

Floating absolute card over the conversation column's top-right (`absolute().top(TITLE_BAR_HEIGHT + 16).right(16).w(ENV_CARD_WIDTH).occlude()`). Owns `Entity<Thread>` and renders the panel body (`render_panel`) which carries the card chrome (border / rounded / shadow / background + `p_3`/`gap_2`) at content height.

> Source: `agent-ui/src/views/context_rail.rs`

#### ContextRailPanel

Panel body (the card's content, content height — no internal scroll surface, though the plan section has its own bounded scroll region). The conversation-info rows (title, status, changes, branch) sit above the usage tree; the plan section renders from cockpit state owned by the rail.

Contents, top to bottom:

- **Header**: bold title (i18n `context-rail-title`) + a [ContextRailCollapseBtn](#contextrailcollapsebtn) ghost button.
- **Status block** (`cockpit_status_block`): a two-line card — phase label (semibold) on line 1, an xs muted elapsed+tokens meta line (i18n `cockpit-run-status-meta`) on line 2. Elapsed refreshes per-second via the thinking ticker.
- **Usage section** (`render_usage_section`): `MemoryStick` icon + "Usage" / "消费" header with cumulative token total, plus cumulative USD cost via `format_cost` when the session carries priced usage (kernel `session_stats` rate-card pricing); a hover tooltip splits main-call vs side-call usage. Then a per-model tree (sorted by total tokens desc, empty for unused models). Each model node carries a tree prefix (`├─` / `└─`) and shows its display name — `provider/model` composite keys, with the `[1m]` context-window suffix resolved from pi registry model metadata (`model_window_tokens`). Tree children per model (indented `│   ` / `    ` + `├─` / `└─`):
  1. **Context budget row**: `{pct}% {used}/{cap}` from `context_budget_pct(window_tokens, effective_context_tokens(...))`; only when the model's window size resolves. Goes warning-colored at ≥90%.
  2. **Token row**: `↑{input} ↓{output} R{cache_read} CH{cache_hit%}` (`--` when there is no input to measure).
  3. **Cost row** (only for priced models): `format_cost(cost)` from `Thread::per_model_cost`.
- **Plan section** (`render_plan_section`, collapsible via `ToggleCockpitTasks` / ctrl/cmd-shift-m, `cockpit_hide_tasks`): the model's execution plan, taken verbatim from the `PlanSnapshot` it publishes via the `UpdatePlan` tool.
- **Changes row**: [ContextRailChangesRow](#contextrailchangesrow).
- **Branch row**: [ContextRailBranchRow](#contextrailbranchrow).
- **Hairline divider**.
- **Sources section**: `Sources` label + "No sources yet" placeholder.

Each numeric cell animates scoreboard-style (`counter_animated`): a fresh `gen` is appended to the animation id on every value delta, so gpui fires a 600ms `ease_out_quint` tween from the previous rendered value to the new one. `env_counter_state: HashMap<String, (u64, u64)>` lives on `ContextRail`, rebuilt every render inside `render_usage_section` to auto-prune cells whose model disappeared.

> Source: `agent-ui/src/views/context_rail.rs` (`render_panel`)

#### ContextRailCollapseBtn

Ghost `xsmall` button in the panel header, `IconName::PanelRightClose`, tooltip i18n `context-rail-collapse`. Folds the rail into a drawer when narrow (the drawer's open affordance uses `context-rail-drawer-open` / `context-rail-expand`).

> Source: `agent-ui/src/views/context_rail.rs`

#### ContextRailChangesRow

Working-tree diff stat line in the panel body. `env_row` with `Frame` icon, "Changes" label, and a trailing `+added` (green) / `-deleted` (red) / `?untracked` (muted) cluster from `GitChangeStats`. Before the first git refresh lands (or when no project is bound) the trailing slot shows `--` / "No project" so the row keeps its height instead of flickering.

Stats come from `git diff --numstat HEAD` (binary rows `-`/`-` skipped) plus `git ls-files --others --exclude-standard` for untracked, shelled out via [`crate::git_status`](#git_status) on the global tokio runtime. Refreshed (debounced 400ms) by `Workspace` on thread attach, terminal `Stop`, and enter/exit worktree.

> Source: `agent-ui/src/views/context_rail.rs` (`render_changes_row`)

#### ContextRailBranchRow

Resolved git identity block in the panel body (`render_branch_block`). When the thread is inside a worktree, a leading worktree-name row precedes the branch row; both rows share the same `h_flex` (icon + label) layout, `text_sm` font, and `gap_2` spacing so they read as peer rows.

- **Worktree row** (rendered only while inside a worktree): lucide `workflow` icon (resolved via [assets](#assets) at `icons/workflow.svg`) + the worktree directory basename as the label. Non-interactive — no trailing, no cursor, no menu.
- **Branch row**: `env_row_clickable` with lucide `git-branch` icon (`icons/git-branch.svg`) — the whole row is a pointer cursor that opens [ContextRailBranchMenu](#contextrailbranchmenu). The label shows:
  - The branch name when on a normal branch.
  - The short sha + "(detached)" hint when in detached HEAD.
  - "Not a git repo" when `git rev-parse --show-toplevel` fails.
  - "git unavailable" when the `git` binary is missing.
  - "--" before the first refresh lands; "No project" when no project is bound.

Both glyphs live in manox's local asset bundle (`ExtrasAssetSource` in `agent-ui/src/assets.rs`), not `gpui-component-assets` — `IconName` is generated at compile time from the latter's directory and cannot reference them, so the rows construct `Icon::default().path("icons/…")` instead of `Icon::new(IconName::…)`. Branch resolution prefers `Thread::worktree().branch` when inside a worktree; otherwise shells out to `git branch --show-current`, falling back to `git rev-parse --short HEAD` for detached HEAD. All via [`crate::git_status`](#git_status).

> Source: `agent-ui/src/views/context_rail.rs` (`render_branch_block`)

#### ContextRailBranchMenu

`PopupMenu` anchored under the branch row, rendered as a `deferred(...).with_priority(1)` overlay so it paints on top of the entire workspace tree and is never occluded by the rail's later-painted siblings (usage/budget/plan rows) nor clipped by the rail's scroll container. Mirrors the title-menu / model-selector pattern: the menu entity + its `DismissEvent` subscription are created lazily on open, dropped on close. Items:

- **Copy branch name** (i18n `workspace-env-git-copy-branch`) — shown when a branch resolved; writes to the clipboard silently.
- **Copy worktree path** (i18n `workspace-env-git-copy-path`) — shown when the thread is inside a worktree.
- **Exit worktree** (i18n `workspace-env-git-exit-worktree`) — shown only inside a worktree, behind a separator; calls `Thread::exit_worktree` (the branch row never exits directly, so a stray click cannot destroy the isolation context).

> Source: `agent-ui/src/views/context_rail.rs` (`render_branch_row`)

#### git_status

Pure parsing + tokio-bridged IO module backing [ContextRailChangesRow](#contextrailchangesrow) / [ContextRailBranchRow](#contextrailbranchrow). Shells out to the system `git` binary (never `git2` — banned by project rule) on the global tokio runtime via `agent::runtime::handle`, delivering results back through an `async_channel` (the same bridge the worktree tool uses).

- `parse_numstat` / `parse_branch` / `parse_short_sha` / `count_untracked` — pure value-type parsers (unit-tested without a real repo).
- `gather` — runs `git rev-parse --show-toplevel`, `git branch --show-current` / `git rev-parse --short HEAD`, `git diff --numstat HEAD`, `git ls-files --others --exclude-standard` in one background task; returns `None` when the cwd is not under git.
- `gather_bridged` — spawns `gather` on the tokio runtime and awaits the result from a gpui `cx.spawn`.

> Source: `agent-ui/src/git_status.rs`

### 3.4 EditorPane

Right-side panel, shown when `editor_open` is true. 640px default (320–960 draggable).

#### EditorDivider

6px drag handle between MainColumn and the right pane (conditional — shown while any right-pane tab is open).

> Source: `agent-ui/src/workspace.rs`

#### RightPane

Vertical flex, right panel. A tab container holding the markdown editor and browser tabs as peer tab types. Visible while `right_tabs` is non-empty; the active tab's content fills the body.

> Source: `agent-ui/src/workspace.rs`

#### RightTabBar

Top-level underline tab bar over `right_tabs`: `[Editor] [browser:url] …`. Selecting a tab switches `active_right_tab`. Browser tabs carry a `×` suffix that closes the tab via `close_right_tab` → `close_browser_tab` (click stops propagation so it does not also select). The Editor tab has no close affordance — it keeps its keyboard toggle (`ToggleEditor` / `CloseEditor`).

> Source: `agent-ui/src/workspace.rs`

#### EditorWriteTab

Plain-text multi-line [InputField](#inputfield) for markdown editing. A second-level Write/Preview toggle lives inside the Editor tab's content area.

> Source: `agent-ui/src/workspace.rs`

#### EditorPreviewTab

Rendered markdown view (`Markdown`).

> Source: `agent-ui/src/workspace.rs`

#### BrowserView

A right-pane tab hosting an untrusted embedded native webview (`RightTab::Browser(BrowserTabId)`, an equal citizen of the right tab bar alongside `Editor`). Chrome row is pure GPUI: back / forward buttons + a single-line address bar whose `Enter` navigates (re-submitting the current URL reloads). The content area is the native `WebViewElement` from `manox-webview`, which tracks the gpui layout via `set_bounds`. Built with `TrustMode::Untrusted`: only the closed-enum notify bridge and the inbound-write request bridge are injected — the page has no Tauri command surface. The process-wide bridges attach at build via `WorkspaceBrowserHost::attach_to_builder`; the host itself is installed once at startup in `main` (`WorkspaceBrowserHost::install`) and routes notifications back to their tab. Tabs are opened via the `OpenBrowserTab` action (`cmd-b`) and closed via the tab's × affordance or `CloseBrowserTab` (`cmd-shift-b`, closes the active browser tab). `tab_id`s are process-unique and woven into the webview label so the host can route inbound notifications back to their tab. The inbound-write authorization overlay (manox's `InboundWriteOverlay`) was retired with the manox harness; `ThreadEvent::InboundAuthorization` still exists but has no UI consumer today.

Two transient banners render between the chrome row and the content area, both driven by flags the `BrowserHost` sets on the view (cleared on navigation / resolution):

- **Yield banner** — shown while a `web_explore_yield` call is parked. A "Done" button resolves the parked Task via `WorkspaceBrowserHost::resolve_handback` (the page-side `user_handback` notify is ignored by design — an untrusted page must not resume a parked yield). Retired by the "Done" click, by navigation, or by Stop/Error cleanup (`clear_yields_for_thread`).
- **Read hint** — a muted one-liner shown after `read_text` / `read_dom` / `screenshot` / `eval_script` extracts content from an `https://` origin, signalling that logged-in page content was exposed to the agent.

> Source: `agent-ui/src/views/browser_view.rs`


## 4. ViewMode::Settings

Full-window settings page rendered through the shared [WorkspaceShell](#workspaceshell) (`SettingsLeftNav | divider | main`), so the sidebar divider stays draggable exactly like the app page. Slides in from left (180ms), slides out to right (200ms).

#### ManagementBackControl

Unified "back to app" control — `ArrowLeft` + label row (px_2/py_1p5/gap_2, accent hover wash, `theme.radius`). Mounted as the first row of the [SettingsLeftNav](#settingsleftnav) pinned top slot (above the search input and group list) so the back affordance reads as a peer of the sidebar menu items, not an isolated button. The settings page no longer ships a shared management TitleBar — each management surface reuses the app-page scaffold (sidebar + overlay TitleBar in the main column), and the back control lives in the sidebar.

> Source: `agent-ui/src/views/management_shell.rs`

#### SettingsView

Settings page state + renderers. `render_nav` produces the sidebar slot element and `render_main` the main-column element; the Workspace mounts both into the shared shell. Holds the sidebar `width` (synced from `Workspace::sidebar_width` by the divider drag, and seeded on entry) so the settings sidebar resizes exactly like the app sidebar. The main column is a relative `v_flex` with an absolute [SettingsTitleBar](#settingstitlebar) overlay on top and [SettingsRightPane](#settingsrightpane) content below `pt(TITLE_BAR_HEIGHT)`.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsTitleBar

Absolute-positioned `TitleBar` overlay (`h(TITLE_BAR_HEIGHT)`, `top_0/left_0/right_0`) in the settings main column — same chrome as the conversation column's TitleBar. Pure window-drag region: it renders no text (the selected item's identity is carried by each panel's own big page heading, mirroring ChatGPT.app Settings). Carries macOS traffic-light avoidance. No back button — back lives in [SettingsLeftNav](#settingsleftnav).

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsLeftNav

Settings sidebar (`bg:background`, right border) rendered at the shared sidebar width; no standalone TitleBar, the macOS traffic-light buttons float over its transparent top (`pt(top_inset)`, 28px on macOS / 8px elsewhere). The back control + search input live in a pinned top slot that never scrolls; only the [SettingsGroupList](#settingsgrouplist) scrolls (`overflow_y_scroll`) beneath them.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsSearchInput

Search/filter input in left nav.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsGroupList

Scrollable list of settings groups with section headers.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsGroup

A labeled group of settings items. Groups: General, Integrations, Coding, External Tools, Archived.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsItem

Single settings row: icon + label, clickable, highlights when selected. An item may carry an optional `brand_icon` (an embedded brand SVG asset path, e.g. `icons/chatgpt.svg`) rendered via `Icon::default().path(...)` in preference to the `IconName` (same mechanism as the sidebar's external-session rows); the External Tools → ChatGPT.app item uses it.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsRightPane

Right content area, dispatches to panel renderers. Each panel/content view owns its own scroll and padding.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsPanel

A specific settings panel rendered in the right pane. Implemented panels: General, Config, Models (the cx provider config editor, see [SettingsModelsPanel](#settingsmodelspanel)), Personalization, Environment, ChatGPT.app (External Tools, see [SettingsChatGptAppPanel](#settingschatgptapppanel)). Every other left-nav item (Appearance, Pets, Keyboard, Snapshots, Plugins, Browser, Computer, Hooks, …) renders the shared "Coming soon" placeholder.

> Source: `agent-ui/src/views/settings/panels.rs`

#### SettingsModelsPanel

Settings → General → Models: two-column form editor for the cx provider config (`~/.config/cx/cx.providers.config.yaml`). Left column is a tree nav: provider nodes (double-click header renames inline) whose expanded children are the four module names — 基本信息 / 环境变量 / 端点配置 / 模型列表 — and a dashed 「+ 添加 Provider」 button at the list end; clicking a module child selects (provider, module) and the wide right column renders that module's form in a bordered panel: 基本信息 (API Key kind dropdown/value pair), 环境变量 (indented key/value rows with per-row 「-」/「+」), 端点配置 (one card per endpoint) or 模型列表 (one card per model, 手动配置 / 自动获取 tab). Add-item buttons are dashed full-width and sit at the end of their lists. Remove controls are uniform 「-」 buttons with two-step confirmation (first click arms with a danger tint, second deletes); block-level removes (model / endpoint) sit outside the block's right edge, vertically centered; selected tree items use `theme.info` text; form blocks carry no background fill; the 手动配置 / 自动获取 tabs underline the active choice in `theme.info`; double-click provider rename exits on blur, Enter or mouse-down-out and autosaves. Every edit debounces into an autosave: validate, atomically write the whole config back (top-level `agents:` preserved verbatim), then reload the provider registry off the main thread. Autosave is disabled while the file fails to parse. Endpoints are unique per Wire API and displayed as Anthropic Messages / OpenAI Responses / OpenAI Completions; `agents:` filters are badge pickers fed by a dropdown (empty selection = all agents); supports_tools / supports_images echo their effective defaults instead of an unset state.

> Source: `agent-ui/src/views/settings/models.rs`

#### SettingsChatGptAppPanel

Settings → External Tools → ChatGPT.app: visualizes and edits the ChatGPT.app injection settings (`cx_providers::ChatGptAppSettings`, top-level `chatgpt_app:` section of `cx.providers.config.yaml`, shared by the CLI and GUI launch paths). Visual language mirrors ChatGPT.app Settings: a big page heading (`text_xl`, the TitleBar renders no text), then per-block name (14px foreground, non-bold) + muted description left-aligned **above** a border-only rounded card (no fill) whose rows are separated by hairlines; each row is two-line (name foreground + description muted, left) with the value right-aligned. Four blocks: **Codex Home** (read-only CODEX_HOME value with copy / reveal-in-Finder), **Model Injection** (display nickname input — replaces the injected provider name when set, whatever provider is launched — plus the injection mode as a segmented two-choice — model list via CDP vs single model via the official config.toml mechanism, active segment a filled pill / inactive plain muted text, with the CDP risk note as the row's inline description — plus a read-only Providers & LLMs catalog, one two-line row per provider, per-provider fetch failures shown as a "failed to load" row rather than omitted), **Variable Injection** (custom env key/value rows with add/remove; reserved keys rejected on save), **More Settings** (`supports_websockets` switch, default false). Editable items autosave through the same debounced touch/save_generation mechanism as the Models panel; the Models panel carries `chatgpt_app:` over from a fresh disk read on save so the two panels never clobber each other. Launch args and the CDP script injection are internal mechanics and are not surfaced.

> Source: `agent-ui/src/views/settings/chatgpt.rs`

#### SettingsSectionCard

Rounded container, `bg:secondary`, holds rows with hairline dividers.

> Source: `agent-ui/src/views/settings/panels.rs`

#### SettingsRow

Single row: title (left) + control (right), optional description.

> Source: `agent-ui/src/views/settings/panels.rs`

#### SettingsSectionHeader

Small bold label for a subsection.

> Source: `agent-ui/src/views/settings/panels.rs`

#### SettingsHairline

1px divider between rows.

> Source: `agent-ui/src/views/settings/panels.rs`

---

## 5. PluginManager (in Settings)

The plugin/skill management UI (PluginManagerView, tab bar, marketplace/plugin/skill cards) was retired with the manox harness. The Settings → Plugins item remains in the left nav and renders the shared "Coming soon" placeholder; the backend registry (`agent::plugin::PluginManager`) stays in the agent crate. MCP server management has its own panel: Settings → MCP servers lists the merged config (mcp.toml + plugin declarations) with live connection state and persistent per-server switches (`[mcp] disabled` in settings.toml, applied on next launch).
Plugin management lives under Settings → Plugins (`PluginManagerView`): a Marketplace tab (add/refresh/remove marketplaces, browse + install their plugins) and a Plugin tab (installed set with update/enable/disable/uninstall), all async with a busy spinner + notice banner; registry changes apply on restart (notice texts say so). Skill authoring and mcp.toml server authoring tabs from the retired harness are not ported yet (need shared-layer write APIs).
---

## 6. ViewMode::Terminal

Full-window terminal emulator, rendered through the shared [WorkspaceShell](#workspaceshell): sidebar + draggable divider + a [TerminalColumn](#terminalcolumn) (TitleBar over the full-bleed `TerminalView`). The terminal view owns its PTY and grid; the workspace only mounts it. Resize/scrollback/selection are handled inside `TerminalView` / `TerminalElement`.

#### TerminalView

Root view, `size_full`. Owns the focus handle; `focus(&self, window, cx)` is called after spawning/attaching/switching an external-agent session so the TUI receives keystrokes immediately. The focused root intercepts `tab` / `shift-tab` (when no search overlay is open) and forwards them to the PTY as `\t` / `\x1b[Z` with `stop_propagation`, so tab never escapes into GPUI focus traversal. Mouse-wheel events are forwarded to the PTY as xterm mouse reports when a TUI app captures the mouse (e.g. claude code / vim / htop), so its own viewport scrolls; on the alt screen without mouse capture the wheel becomes arrow-key presses (xterm alternateScroll, DECRST 1007 permitting); otherwise the local scrollback scrolls. Click count picks selection granularity (1 = char, 2 = semantic word, 3 = line). Hovering text tracks a target — OSC 8 hyperlink span first, else a semantic word that looks like a URL or a path (`:` is not a word separator, so URLs hover whole): the grid underlines the span, a tooltip anchored under the span shows the target text, and cmd/ctrl+click opens it (URLs in the browser; paths revealed in the file manager — directories open, `~` expands, relative paths resolve against the terminal cwd). Overlay chips: a starting indicator at the top right until the shell/agent TUI reports ready (OSC 6973 marker tap, output-quiet window, or fallback timeout), and the foreground process name at the bottom right while something other than the shell owns the foreground process group (1s poll). OSC 10/11/12 color queries are answered from the active theme. The cursor blinks per the `cursor_blink` setting (`off` / `on` / `terminal` = follow the program's DECSET 12/DECSCUSR flag) on a 530ms phase timer; selection, IME preedit, and input within the last 500ms pin it visible. A 2px scrollbar (8px hit area) shows at the right edge while scrollback exists; click/drag maps the y fraction onto the display offset, sharing `display_offset` with wheel/vi scrolling.

> Source: `terminal-ui/src/terminal_view.rs`

#### TerminalTabBar

Tab bar for multiple terminal tabs.

> Source: `terminal-ui/src/terminal_view.rs`

#### TerminalGrid

Monospace grid renderer, `flex_1`. Shapes text runs per line through a content-fingerprint cache (`layout_cache::LineShapeCache`, keyed by alacritty grid line + FNV-1a over each line's cells) so frames that repaint unchanged lines skip `shape_line`; a theme switch clears the cache and a per-frame sweep bounds it to the visible window. The cursor glyph honors the program's DECSCUSR shape (block / underline / beam / hollow-block / hidden) and is skipped on blinked-out phases. The scrollbar track/thumb quads paint here; the element writes the track bounds back to the view for hit-testing.

> Source: `terminal-ui/src/terminal_view.rs`

---

## 7. Shared Primitives

Reusable UI elements from `gpui_component` and `manox-components` used across all views.

#### Button

Clickable button with variants (primary, ghost, outline, danger, etc.).

#### Input / InputState

Text input field (single-line or multi-line auto-grow).

#### PopupMenu

Dropdown menu with items, submenus, separators, labels.

#### PopupMenuItem

Single menu row: icon + label + optional trailing.

#### TabBar

Horizontal tab bar with selectable tabs.

#### Tag

Small colored chip/badge with variant colors.

#### TerminalPanel

First-party selectable text panel (`manox-components::markdown::TerminalPanel`, an `Entity` + `Render` in the `markdown` module) that renders tool output as a terminal-styled shell — **not** a real terminal (no PTY, no grid; `crates/terminal`/`TerminalView` are not involved). One persistent `Entity<TerminalPanel>` is owned per `ToolCallItem` (live + reloaded history) so the document-level `DocSelection` and its `FocusHandle` survive across re-renders: a drag started on frame N keeps its anchor on frame N+1, and Cmd/Ctrl+C reaches a stable focus handle — the same persistence fix that makes assistant/thinking text selectable. The panel renders **only the body**: a transparent, untinted vertical flex (no background fill on the content — it blends into the message list; mono font at `text_sm`（代码档，与代码块同号；thinking body 走 markdown 正文档 `text_base`）; `px_3 py_2`; `cursor_text`) that mounts a zero-size sentinel first, then a single `RichText` document composed of a prompt block + the body. The prompt block appears **only for `bash`** — the one tool that runs a real shell command a human would type in a terminal; internal tools (`grep`/`read_file`/`edit_file`/`glob`/`list_directory`/`monitor`/…) and MCP tools are manox abstractions, not terminal commands, so they render the body only (no cwd / `❯` preamble that would imply "run this in a shell"). The `bash` prompt block: line 1 cwd (home `~`-collapsed) + `git:{branch}` + status markers (`*mod ✘del !conflict ?untracked`, zero counts and the whole git segment omitted when not a repo); line 2 `❯` (green) + the echoed command. **Three-way text styling** separates prompt chrome / input / output by color × slant: guidance (cwd / `git:` / branch / markers / `❯`) — foreground + **upright**; the echoed command — foreground + **italic**; the body (output) — muted + **italic**. The doc div's `.italic()` sets the base the `RichText` unstyled ranges inherit (so command output / file content / diff context all read muted + italic); the prompt-block guidance and command runs pin their own slant via `styled(color, italic)` so the body's inherited italic does not leak into the prompt. The body is rendered per a `PanelKind` chosen by the agent-ui layer from the tool name (`tool_panel_body`): `File` (`read_file`/`write_file`) — a sequential line-number gutter (the agent-ui layer pre-strips the hashline `[path#TAG]` header + `N:` prefixes for `read_file` and feeds the written `content` for `write_file`, so the panel just numbers the content lines 1..N); `Diff` (`edit_file`) — `+`/`-` lines green/red, `@@` hunk headers cyan, `[path#TAG]`/`---` separators muted; `Plain` (default, `bash` + everything else) — `vte::ansi::Processor` parses SGR foreground/bold/italic into `HighlightStyle` ranges (control bytes stripped from the plain text, truecolor/256/16-color resolved; background SGR tracked but not painted). The whole document wraps at panel width (long lines wrap, no horizontal blow-out) and is one continuous selection across prompt + output. The terminal chrome — a **titlebar** showing the command summary (`gh issue create` for `bash`, `read_file path` otherwise) + status + disclosure chevron, click-to-toggle the body — lives in the agent-ui header (`render_tool_entry` / `render_tool_call`): the titlebar and this body share one bordered rounded frame so the pair reads as a single terminal window (titlebar gets a `border_b_1` separator only while the body is shown). Selection supports double-click word (a click inside a registered inline-code span selects the whole span), triple-click line, drag-extend, and Cmd/Ctrl+C copy — shared with `Markdown` via `DocSelection`. Git state is snapshotted per `bash` panel by the agent-ui layer via a background `git status --porcelain` + `git rev-parse --abbrev-ref HEAD` probe keyed off the thread cwd (internal tools skip the probe — they render no prompt block). `render_tool_output` returns `item.panel.into_any_element()` when mounted, falling back to a fenced code block otherwise.

**Pagination.** A finalized body renders `PAGE_SIZE` (20) lines at a time; a "load more" affordance below the body (a centered `ChevronDown` + `+N` count, top-bordered, hover-tinted) grows the window by another page via `show_more`, clamped to the total. Streaming bodies render the whole live output (no pagination); on the streaming→finalized transition the cursor resets to the first page so the result opens at the top. The panel has **no internal vertical scroll** — the message-list `message-list` div scrolls the whole panel — so `show_more` never touches a scroll handle: growing the window appends lines below the current viewport without jumping to the tail. The pixel-anchored, tail-following message-list arbitration (recomputed each frame in `on_prepaint`) keeps the viewport at the user's reading position across the growth, so successive "load more" clicks stay anchored to the current line rather than snapping to the end.

> Source: `components/src/markdown/terminal_panel.rs` · wired by `agent-ui/src/views/message.rs` (`tool_panel_body` → `ensure_tool_panel` / `sync_tool_*_panel` / `rebuild_tool_panels`, titlebar frame in `render_tool_entry` / `render_tool_call`) + `agent-ui/src/conversation.rs` (`apply` ToolOutput/ToolResult arms, `rebuild_from_messages`)

#### Markdown

Self-built stateful markdown renderer (`manox-components::markdown::Markdown`, an `Entity` + `Render`) replacing `gpui_component::TextView::markdown`. Owns the source + an `IncrementalParser` (parse-once: freezes the completed prefix so a streaming append only re-parses the growing tail) + a document-level `DocSelection`. The `Render` builds a focusable vertical flex root mounting a zero-size sentinel as the first child — the sentinel clears the per-frame block registry at paint start, then each block's `RichText` re-registers its geometry during paint; the root's mouse listeners hit-test that registry to drive one continuous selection across paragraph / code / list boundaries, and the key listener copies it on Cmd/Ctrl+C. Click semantics: single click places the anchor + starts a drag; double-click selects the word at the click (a click landing inside a registered inline-code span selects the whole span verbatim); triple-click selects the line. `RichText` composes `StyledText` for shaping/glyph-painting and overlays rounded inline-code washes + this block's slice of the shared selection. Block visuals: paragraphs/headings via `StyledText::with_highlights`; code blocks with line-number gutter + `overflow_x_scroll` + tree-sitter highlighting (highlight result cached per `(lang, content_hash)`); unified-diff blocks with accent wash + left bar; GFM tables with column alignment + horizontal scroll; task-list checkboxes; code/diff block hover copy button. Streaming bodies paint plain text + cursor; the full layout mounts once the stream ends.

#### TurnFrame

Shared framed text container (`manox-components::turn_frame::TurnFrame`) used for user turns. It paints one continuous accent-colored stroke path for the door-shaped frame, leaving the bottom center open while preserving rounded `╰─` / `─╯` corners. The lower stroke is lifted slightly into the bottom padding so the open edge visually hugs the final text line without letting markdown content overflow its layout box. The component does not fill the content background, does not rely on masking a complete border, and avoids assembling the frame from independent rail nodes. Callers provide header, trailing controls, and body content.

> Source: `components/src/turn_frame.rs`

#### Icon

Named icon from the icon set (e.g., `IconName::Folder`, `IconName::Search`).

#### BrailleSpinner

Text-based braille-dot spinner cycling through `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` (10 frames, 800 ms cycle). Used wherever an in-progress indicator is needed, replacing the old rotating-circle `Spinner`.

> Source: `agent-ui/src/views/braille_spinner.rs`

#### ScrollHandle / ScrollableElement

Scroll container with custom scrollbar.

#### TitleBar

Standard title bar component from gpui_component.

#### ContextMenu

Right-click context menu.

#### Tooltip

Hover tooltip.

---

## 8. Approval Modes

Visual states of the [AccessChip](#accesschip).

#### AutoPilot

Blue — a safety reviewer automatically approves safe tool calls; risky ones are denied (default).

#### Danger

Red — bypass all approvals, bash runs outside the sandbox.
---

## 9. Tool Call Statuses

States of a [ToolCallCard](#toolcallcard).

#### PendingApproval

Waiting for user, surfaces as the [AskDrawer](#askdrawer) question card.

#### Running

Braille-dot spinner or shimmer.

#### Success

Green check, collapsible output.

#### Error

Red X, error output.

#### Denied

Greyed out, "denied" label.

---

## 10. Reasoning Effort Levels

Thread-level reasoning effort. No selector UI in the pi path today — the value is inherited across `/new` (`archive_current_thread_inheriting`) and set via the engine facade; High / Max remain the canonical values.

#### High
#### Max
