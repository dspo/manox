# UI Map

Shared vocabulary for every named UI component in manox. When discussing UI, reference
component names from this file so both parties refer to the same thing.

Component names use PascalCase. The hierarchy mirrors the visual containment tree.

---

## 索引

### 顶层

- [Window](#window) · [Workspace](#workspace) · [ViewMode](#viewmode) · [ViewMode::Workspace](#viewmodeworkspace-layout) · [ViewMode::Settings](#viewmodesettings) · [ViewMode::Terminal](#viewmodeterminal) · [ViewMode::ExternalSession](#viewmodeexternalsession)

### Sidebar

- [Sidebar](#sidebar) · [SidebarScrollBody](#sidebarscrollbody) · [SidebarMenuSection](#sidebarmenusection) · [SidebarMenuItem](#sidebarmenuitem) · [SidebarProjectsSection](#sidebarprojectssection) · [SidebarProjectGroup](#sidebarprojectgroup) · [SidebarConversationsSection](#sidebarconversationssection) · [SidebarNewSessionMenu](#sidebarnewsessionmenu) · [SidebarThreadItem](#sidebarhreaditem) · [SidebarExternalSection](#sidebarexternalsection) · [SidebarExternalSessionItem](#sidebarexternalsessionitem) · [SidebarDivider](#sidebardivider)

### MainColumn

- [MainColumn](#maincolumn) · [TitleBar](#titlebar) · [TitleBarThreadTitle](#titlebarthreadtitle) · [TitleBarGoalChip](#titlebargoalchip) · [TitleBarMenuButton](#titlebarmenubutton) · [Body](#body)

### ContextRail

- [ContextRail](#contextrail) · [ContextRailPanel](#contextrailpanel) · [ContextRailAgents](#contextrailagents) · [ContextRailCollapseBtn](#contextrailcollapsebtn) · [ContextRailChangesRow](#contextrailchangesrow) · [ContextRailBranchRow](#contextrailbranchrow) · [ContextRailBranchMenu](#contextrailbranchmenu)

### Hero

- [Hero](#hero)

### MessageArea

- [MessageArea](#messagearea) · [MessageList](#messagelist) · [MessageItem](#messageitem)

### MessageItem 变体

- [UserMessage](#usermessage) · [AssistantMessage](#assistantmessage) · [ReasoningBlock](#reasoningblock) · [ThinkingStatusRow](#thinkingstatusrow) · [ToolCallCard](#toolcallcard) · [AgentTaskCard](#agenttaskcard) · [ErrorMessage](#errormessage) · [NoticeMessage](#noticemessage) · [RecapCard](#recapcard) · [RetryBadge](#retrybadge)

### Footer / Composer

- [Footer](#footer) · [Composer](#composer) · [QueuedFollowUps](#queuedfollowups) · [ComposerDivider](#composerdivider) · [AttachmentChips](#attachmentchips) · [AttachmentChip](#attachmentchip) · [ComposerInputRow](#composerinputrow) · [InputField](#inputfield) · [SendBtn](#sendbtn) · [ModelChip](#modelchip) · [AccessChip](#accesschip) · [EffortChip](#effortchip) · [ProjectChip](#projectchip) · [ModeChip](#modechip) · [PlusBtn](#plusbtn) · [TeamChip](#teamchip)

### AskDrawer

- [AskDrawer](#askdrawer) · [AskDrawerHeader](#askdrawerheader) · [AskDrawerQuestion](#askdrawerquestion) · [AskDrawerOptions](#askdraweroptions) · [AskDrawerOtherInput](#askdrawerotherinput) · [AskDrawerResponseInput](#askdrawerresponseinput) · [AskDrawerNav](#askdrawernav)

### Popups & Dropdowns

- [PlusMenu](#plusmenu) · [CompletionPopover](#completionpopover) · [ModelMenu](#modelmenu) · [AccessMenu](#accessmenu) · [EffortMenu](#effortmenu) · [ProjectMenu](#projectmenu) · [TitleMenu](#titlemenu) · [GoalPopover](#goalpopover)

### Overlays

- [ApprovalOverlay](#approvaloverlay) · [InboundWriteOverlay](#inboundwriteoverlay) · [BlankProjectOverlay](#blankprojectoverlay)

### EditorPane

- [EditorDivider](#editordivider) · [RightPane](#rightpane) · [RightTabBar](#righttabbar) · [EditorWriteTab](#editorwritetab) · [EditorPreviewTab](#editorpreviewtab) · [MemberTab](#membertab) · [MemberPanel](#memberpanel) · [SubagentPanel](#subagentpanel) · [BrowserView](#browserview) · [PlanPreviewTab](#planpreviewtab)

### Composer (team)

- [TeamChip](#teamchip) · [TeamDrawer](#teamdrawer)

### ManagementShell

- [ManagementBackControl](#managementbackcontrol)

### Settings

- [SettingsView](#settingsview) · [SettingsTitleBar](#settingstitlebar) · [SettingsLeftNav](#settingsleftnav) · [SettingsSearchInput](#settingssearchinput) · [SettingsGroupList](#settingsgrouplist) · [SettingsGroup](#settingsgroup) · [SettingsItem](#settingsitem) · [SettingsRightPane](#settingsrightpane) · [SettingsPanel](#settingspanel) · [SettingsSectionCard](#settingssectioncard) · [SettingsRow](#settingsrow) · [SettingsSectionHeader](#settingssectionheader) · [SettingsHairline](#settingshairline)

### PluginManager

- [PluginManagerView](#pluginmanagerview) · [PluginManagerTabBar](#pluginmanagertabbar) · [PluginManagerNoticeBanner](#pluginmanagernoticebanner) · [PluginManagerBusyIndicator](#pluginmanagerbusyindicator) · [PluginManagerTabContent](#pluginmanagertabcontent) · [MarketplaceTab](#marketplacetab) · [PluginTab](#plugintab) · [SkillTab](#skilltab) · [McpTab](#mcptab) · [PluginCard](#plugincard) · [SkillCard](#skillcard) · [McpServerCard](#mcpservercard) · [FormCard](#formcard)

### Terminal

- [TerminalView](#terminalview) · [TerminalTabBar](#terminaltabbar) · [TerminalGrid](#terminalgrid)

### Shared Primitives

- [Button](#shared-primitives) · [Input](#shared-primitives) · [PopupMenu](#shared-primitives) · [PopupMenuItem](#shared-primitives) · [TabBar](#shared-primitives) · [Tag](#shared-primitives) · [Markdown](#markdown) · [TerminalPanel](#terminalpanel) · [TurnFrame](#turnframe) · [Icon](#shared-primitives) · [ScrollHandle](#shared-primitives) · [TitleBar](#shared-primitives) · [ContextMenu](#shared-primitives) · [Tooltip](#shared-primitives)

### 状态

- [ApprovalMode](#approval-modes) · [ToolCallStatus](#tool-call-statuses) · [ReasoningEffort](#reasoning-effort-levels)

---

## 1. Window

#### Window

Top-level native window, title "manox", min 900×600.

> Source: `manox/src/main.rs`

## 2. Workspace

#### Workspace

Root container, horizontal flex (`h_flex`), owns all sub-views.

> Source: `agent-ui/src/workspace.rs`

### 2.1 ViewMode

`Workspace` switches between four mutually exclusive full-window modes:

#### ViewMode::Workspace
Default — sidebar + conversation + composer.

#### ViewMode::Settings
Full-window settings overlay with slide-in animation. Plugin/skill/MCP management lives under Settings → Integrations → Plugins (rendered in the right pane, not a separate mode).

#### ViewMode::Terminal
Full-window terminal emulator.

#### ViewMode::ExternalSession
Full-window external agent CLI session (claude / codex / copilot). Renders the active `ExternalSession`'s `TerminalView` (the agent's TUI) in place of the conversation, with a TitleBar showing the agent's live OSC title (falling back to the kind label — "Claude Code" / "Codex" / "GitHub Copilot") + provider/model. The session has no dedicated titlebar close button: it is archived the same way a thread row is — via the hover archive control on its [SidebarExternalSessionItem](#sidebarexternalsessionitem) row (or the title menu). Lives in memory only — never persisted; the sidebar row is removed both on archive (kill + drop the `SessionHandle`) and when the CLI exits on its own (a `ChildExit` subscription on the terminal tears the session down without user action). The terminal's OSC title is mirrored into `ExternalSession.title` so the titlebar and sidebar row share one `display_title()`. If the removed session was the active one, the view falls back to the conversation pane.

---

## 3. ViewMode::Workspace Layout

The default mode. Two top-level slots divided by a 6px [SidebarDivider](#sidebardivider): left = [Sidebar](#sidebar), right = the middle column — a relative `v_flex` that holds a shared [TitleBar](#titlebar) overlay on top (spanning the whole middle column) and the conversation column underneath. The [ContextRail](#contextrail) is NOT a flex sibling column — it is an absolute overlay floating over the conversation column's top-right (`absolute().top(TITLE_BAR_HEIGHT + 16).right(16).w(ENV_CARD_WIDTH).occlude()`), content height (never full-height), with the conversation body reserving `ENV_CONTENT_INSET` right padding so the message list never hides behind the card. [EditorPane](#editordpane) opens as a third top-level column to the right of the middle column when any right-pane tab is active; while it is open the card stays hidden so the conversation reclaims its width. The card also folds away below `RAIL_NARROW_BREAK` (900px middle-column width), in which case the conversation column fills the middle column.

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

### 3.1 Sidebar

Left panel, fixed width (260px default, 200–480 draggable).

#### Sidebar

Full-height left panel, vertical flex, `bg:background`, right border.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarScrollBody

Scrollable body inside Sidebar, `overflow_y_scroll`.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarMenuSection

Top section: New Thread, Search, Scheduled menu items.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarMenuItem

Single menu item, icon + label, clickable.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarProjectsSection

Middle section: project-grouped threads (if any projects exist).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarProjectGroup

Collapsible folder: chevron + folder icon + project name, indented thread list.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarConversationsSection

Bottom section: loose (non-project) threads. The header's `+` button opens the `SidebarNewSessionMenu` popup.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarNewSessionMenu

`PopupMenu` anchored below the "Conversations" header `+`. One flat row (Manox → `NewThread`) plus one `submenu_with_icon` per external agent kind (Claude Code / Codex / GitHub Copilot). All four top-level rows use the menu component's native icon slot with a monochrome brand SVG, keeping their icon and label columns aligned. Each agent submenu is a provider→model cascade: models from `registry::global().models()` filtered by the agent's `visible_agents()`, grouped by `provider_name()` into provider submenus, each listing its supported models. Picking a model emits `SpawnExternalSession(kind, provider, model)`. An agent with no supporting model renders a muted "no model configured" label row instead of provider submenus.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarThreadItem

Unified row projection (`SidebarThreadItem` struct: `id`/`display_id`/`title`/`updated_at`/`project`/`selected`/`running`/`icon`/`archive_action`) rendered by one `render_thread_item` for both native threads and external-agent sessions. Native thread row: unread red dot (8px, `theme.danger`, shown when `summary.has_unread`), pinned star, title, short-id tag (shimmer if running), token count, time, hover archive btn. Hover/active/selected wash uses the thread's last saved approval-mode color. The red dot marks a thread that finished a turn while the user was viewing another thread; it clears when the user switches into the thread. External rows reuse the same layout but swap the leading icon for the agent's brand SVG and drop the unread/pin/token/running shimmer affordances. The row kind (`RowKind::Thread { archived }` vs `RowKind::External`) routes the hover archive button to the right `SidebarEvent` (`ArchiveThread` / `ArchiveExternalSession`).

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarExternalSection

Section below "Conversations" listing live external agent sessions (claude / codex / copilot). Rendered only when at least one `ExternalSession` is live. The sidebar holds a `Vec<ExternalSessionSummary>` projection pushed by the Workspace — it never owns the PTY-bearing `ExternalSession` structs.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarExternalSessionItem

Single external-session row, rendered through the unified `render_thread_item` (see [SidebarThreadItem](#sidebarthreaditem)) with `RowKind::External`. Leading icon is the agent's brand SVG (`claude.svg` / `codex.svg` / `githubcopilot.svg`); title is `display_title()` (the agent's OSC title, falling back to the kind label); the short-id tag shows the cx session id (derived from the `<id>.sock` filename of the session's socket path) with click-to-copy of the full id / socket path. The row has no trailing `×` — it shares the thread row's hover archive button, which emits `ArchiveExternalSession(id)` (kill + drop the `SessionHandle`, the unified archive semantics). Clicking the row emits `OpenExternalSession(id)`. External rows live in their own section and don't participate in the conversation selection slide.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarDivider

6px drag handle between Sidebar and MainColumn, `cursor:col-resize`.

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

#### TitleBarGoalChip

Goal status chip (visible when thread has a goal), click → [GoalPopover](#goalpopover).

> Source: `agent-ui/src/workspace.rs`

#### TitleBarMenuButton

"..." button → opens [TitleMenu](#titlemenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### Body

Vertical flex below TitleBar, `pt:TITLE_BAR_HEIGHT`, houses [Hero](#hero) or [MessageArea](#messagearea) + [Footer](#footer).

> Source: `agent-ui/src/workspace.rs`

#### 3.2.1 Hero

Shown when the thread has no substantive messages.

#### Hero

Vertically centered welcome area: logo/heading + inline [Composer](#composer).

> Source: `agent-ui/src/workspace.rs`

#### 3.2.2 MessageArea

Shown when the thread has messages. Replaces [Hero](#hero).

#### MessageArea

Wraps [MessageList](#messagelist).

> Source: `agent-ui/src/workspace.rs`

#### MessageList

Pixel-anchored `ScrollHandle` ordinary scroll container (`div().overflow_y_scroll().track_scroll(&message_scroll)`); the scroll position is an absolute pixel offset, not an item-index + intra-item delta, so streaming growth, width-driven reflow, and streaming→finalized body switches cannot "fly to the top" — the viewport holds its pixel position. Tail-follow is arbitrated each frame from `max_offset()`/`offset()`: when the user sits at the bottom, `auto_follow` latches true and `scroll_to_bottom()` runs on each append; when scrolled up, the pixel offset is left untouched so a resize or a still-streaming tail never yanks the reader back. No `gpui::list` / `ListState` / `FollowMode` / `splice` / `remeasure_items` machinery — that index-anchor compensation layer is gone.

> Source: `agent-ui/src/workspace.rs`

#### MessageItem

Single rendered conversation item, centered, max-width 760px. Each `MessageItem` renders one of the variant cards below based on `ConvItem` kind.

> Source: `agent-ui/src/views/message.rs`

##### MessageItem variants

#### UserMessage

Full-width user turn block rendered inside [TurnFrame](#turnframe): `You > Time/DateTime > ModelID` metadata header, selectable markdown body, copy btn (hover), and an approval-mode-colored frame captured at send time.

> Source: `agent-ui/src/views/message.rs`

#### AssistantMessage

Full-width block: role label + copy btn + markdown body (plain text while streaming).

> Source: `agent-ui/src/views/message.rs`

#### ReasoningBlock

Collapsible: chevron + "Reasoning" label + left-bordered muted body. Each reasoning round (an `ActivityEntry::Reasoning` inside a `Thinking` segment, plus the top-level `ConvItem::Reasoning`) owns a persistent `Entity<Markdown>` (`markdown` field) mounted on first sync — so drag-select + Cmd/Ctrl+C survive across frames (a per-frame `Entity` would reset the `DocSelection`/`FocusHandle` every render and break selection on reasoning text the same way it did on tool output). Italic styling propagates from the row's `Markdown::italic` toggle.

> Source: `agent-ui/src/views/message.rs`

#### ThinkingStatusRow

Folded batch of tool calls from one model response, rendered as one Claude Code–style status line. Header: spinner (live) or static dot (frozen) + "Thinking for Xs"/"Thought for Xs" label + aggregated action counts ("reading 2 files, running 1 shell command…") + chevron. Collapsed body shows only the most recent `⎿` entry; expanded lists every entry. Each `⎿` entry (`render_activity_entry`) is a one-line summary (status icon + tool title, mono) that expands to its full tool output via `render_tool_output`. The elapsed counter ticks every second via a gpui background timer spawned on `TurnStarted` and self-terminating on terminal `Stop`/`Error`; `frozen_secs` pins the final value so later re-renders don't inflate it. Ordinary tool calls now fold here instead of producing standalone cards.

> Source: `agent-ui/src/views/message.rs` — `render_thinking`, `render_activity_entry`, `thinking_summary`. Container state: `ConversationState` (`ConvItem::Thinking` / `ThinkingContainer`).

#### ToolCallCard

A standalone tool-call card (`render_tool_call`) for the special-case tools that don't fold into a [ThinkingStatusRow](#thinkingstatusrow) batch — today `agent` sub-agent calls and `AskUserQuestion`. A model response's other tool calls batch into the `Thinking` container; their output renders via [TerminalPanel](#terminalpanel). A proposed plan now renders inline as a [PlanReviewCard](#planreviewcard) drawer card in the message list at turn end, reusing the AskUserQuestion drawer shell with the verdict buttons in the card.

Statuses: `PendingApproval` | `Running` | `Success` | `Error` | `Denied` — see [ToolCallStatus](#tool-call-statuses).

> Source: `agent-ui/src/views/message.rs`

#### AgentTaskCard

Compact, single-line sub-agent row: `[status] type · short title`. Running and pending rows use an animated spinner; terminal rows use check, error, or minus icons. The title is always one line with truncation and a full-title tooltip. It deliberately renders no child text, nested messages, copy control, metrics, or expansion affordance. Clicking the row opens or focuses the corresponding read-only [SubagentPanel](#subagentpanel) in the right pane.

> Source: `agent-ui/src/views/message.rs`

#### ErrorMessage

Rounded card, `bg:danger/0.06`, red text, "Error" label + copy btn.

> Source: `agent-ui/src/views/message.rs`

#### NoticeMessage

Rounded card, `bg:secondary/0.15`, muted text, "Notice" label + copy btn.

> Source: `agent-ui/src/views/message.rs`

#### RecapCard

Collapsible compaction summary card: chevron + book icon + "Context compacted" label + copy btn. Body is the model-generated handoff summary (markdown, not localized). Collapsed by default; emitted on `ThreadEvent::Compaction` and rebuilt from `MessageContent::Compaction` on thread reload.

> Source: `agent-ui/src/views/message.rs`

#### RetryBadge

Amber badge, `bg:warning/0.12`, spinner + "Retry N/M (in Xs)" text.

> Source: `agent-ui/src/views/message.rs`

#### 3.2.3 Footer

Bottom area of MainColumn, below [MessageArea](#messagearea) (or below [Hero](#hero) on first screen).

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

#### EffortChip

Dropdown chip showing [ReasoningEffort](#reasoning-effort-levels) → [EffortMenu](#effortmenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### ProjectChip

Dropdown chip showing current project → [ProjectMenu](#projectmenu) popup.

> Source: `agent-ui/src/workspace.rs`

#### ModeChip

Always-visible chip showing the thread's [CollaborationMode](#collaboration-modes) (`mode-chip-default` / `mode-chip-plan`). Click, `/plan`, or `shift-tab` (the `CycleCollaborationMode` action) cycles Plan↔Default. In Plan mode the tool set is read-only and the model submits a plan via a `<proposed_plan>` block surfaced as a [PlanReviewCard](#planreviewcard) drawer card (verdict buttons in the card); in Default the full tool set is restored.

> Source: `agent-ui/src/workspace.rs` — `render_mode_chip`, `mode_chip_visual`.

#### PlusBtn

"+" button → [PlusMenu](#plusmenu) popup.

> Source: `agent-ui/src/workspace.rs`

##### AskDrawer

Replaces [Composer](#composer) when `pending_ask` is set.

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

#### PlusMenu

Trigger: [PlusBtn](#plusbtn). "Add" menu: files, goal, cycle Plan↔Default, plugins.

> Source: `agent-ui/src/views/composer_menu.rs`

#### CompletionPopover

Trigger: typing `/` (slash commands) or `@` (skills + subagents) at the caret in [InputField](#inputfield). A typeahead list anchored above the composer: filters live on every keystroke, navigated with up/down, confirmed with Tab or Enter, dismissed with Escape. While open the composer wrapper sets a `completion = open` key context so the `completion == open > Input` keybindings shadow the Input's own navigation bindings. A pure render overlay — [InputField](#inputfield) keeps focus throughout, so the query keeps filtering as the user types.

> Source: `agent-ui/src/views/completion.rs` (state + detection + rendering), wired in `agent-ui/src/workspace.rs`

#### ModelMenu

Trigger: [ModelChip](#modelchip). Model selector dropdown.

> Source: `agent-ui/src/workspace.rs`

#### AccessMenu

Trigger: [AccessChip](#accesschip). [ApprovalMode](#approval-modes) selector: OnRequest / AutoReview / Yolo.

> Source: `agent-ui/src/workspace.rs`

#### EffortMenu

Trigger: [EffortChip](#effortchip). [ReasoningEffort](#reasoning-effort-levels): Low / Medium / High / XHigh / Max / Ultracode / Auto.

> Source: `agent-ui/src/workspace.rs`

#### ProjectMenu

Trigger: [ProjectChip](#projectchip). Recent projects + create blank / select folder.

> Source: `agent-ui/src/workspace.rs`

#### TitleMenu

Trigger: [TitleBarMenuButton](#titlebarmenubutton). Pin, archive, copy, schedule, new window.

> Source: `agent-ui/src/views/title_menu.rs`

#### GoalPopover

Trigger: [TitleBarGoalChip](#titlebargoalchip). Goal status, elapsed time ticker, evaluation details.

> Source: `agent-ui/src/workspace.rs`

#### 3.2.5 Overlays

Absolute-positioned over [Body](#body), with scrim.

#### ApprovalOverlay

Trigger: tool requires approval. Centered modal + scrim (`bg:foreground/0.6`): tool name, input preview, Deny / Allow Once / Always Allow buttons.

> Source: `agent-ui/src/workspace.rs`

#### PlanReviewCard

Plan-review card rendered in the message list (`ConvItem::PlanReview { plan_text, active }`, `render_plan_review_card`). Pushed `active=true` by a `ThreadEvent::PlanReady` at a terminal stop in Plan mode — the `Workspace` PlanReady handler both pushes the plan body into the conversation as this card and sets `pending_plan_review` (no modal). **Active** state reuses the same drawer shell as [AskUserQuestion](#askuserquestion)'s card (`render_ask_user_card`): `pb_5().mb(px(-10.)).shadow_lg()` so the composer covers the card's tail and it reads as emerging from beneath the composer, a 180ms `ease_out_quint` slide-in animation, and a `PlanDrawer` key context. Layout: an accent-tinted header (dashboard icon + "Plan" label) carrying three ghost icon buttons — download (clipboard fallback) / copy (clipboard) / open-in-side-panel (opens a [PlanPreviewTab](#planpreviewtab)); the plan body as a [Markdown](#markdown) view; and a top-bordered footer action row with two verdicts — Clear-&-Implement (ghost, exit Plan → Default, archive this thread + spawn a fresh one seeded with the plan) / Implement (ghost, exit Plan → Default, inject "Implement the approved plan." user turn — the rail's plan seeds later from the model's first `UpdatePlan` call, not the approved text) — both delegating to `respond_plan_review`. There is no "stay in Plan mode" button: staying is not a verdict — the user simply keeps typing, which dismisses the pending plan (see Dismissed below) and lets the model re-propose. The composer stays live so the user may type to discuss or refine the plan. **Verdict as a user bubble**: an implement verdict retires the ephemeral card and pushes the verdict as a user message carrying the approved plan text (`agent::implement_plan_user_message`) with the same UI metadata (`MessageUiMetadata`) the thread injects — so the live view and a reloaded thread both show this one bubble and no plan card. `Implement` pops the pending card from the tail (`ConversationState::pop_plan_review_tail` — safe because the pending card is the live tail at verdict time) then pushes the bubble to the live conversation and calls `Thread::implement_approved_plan` (seed + run on the current thread). `ImplementClearContext` does not touch the current conversation — it archives this thread and spawns a fresh one: the workspace inherits the old thread's model / reasoning effort / approval mode / cwd / project, constructs a new `Thread`, calls `Thread::seed_approved_plan` (inserts the verdict bubble as the new thread's first user message, no run), `attach_thread`es it (saving the old thread, rebuilding the conversation from the new thread so only the seed bubble shows, wiring the event sub, clearing the input draft), `save_thread(touch=true)` so the new thread appears in the sidebar, then `run_turn`s it — events stream into the live view — and finally `archive_thread(old, true)` removes the old thread from the sidebar's active list (consistent with any archived thread; no "show archived" UI today). The user perceives only that the underlying thread id changed and prior messages vanished. **Dismissed (no verdict)**: a free-form message sent without verdicting means the user is revising, not accepting — `ConversationState::consume_plan_review` demotes the most recent active card to a plain bordered record (header + markdown body, no drawer shadow/slide, no verdict footer) so the plan stays readable while the user discusses it but cannot be re-judged. The dismissed plan is also persisted as a UI note (`UiNoteKind::PlanReview`, `thread_ui_notes`) anchored to the user message that triggered the plan's turn, so `rebuild_from_messages` splices the collapsed card back at that turn's end — ahead of the dismissing message — and the plan survives a thread switch and a full reload (the live card is otherwise UI-only and never enters `Thread::messages`). Only the active (still-pending, undecided) plan is not persisted: it is stashed in-memory across a thread switch (see below) and resets on restart, mirroring `collaboration_mode`'s session scope. The `<proposed_plan>` block content never enters the model-facing message history; only the verdict bubble (implement) or the user's own typed message (dismiss) persists, plus the UI note reproducing the dismissed card. Because the active pending plan text would otherwise vanish when switching threads, `Workspace` keeps a per-thread `pending_plans` stash (mirroring `drafts`): switching away from a thread with a pending verdict stashes the plan, and switching back re-pushes the card (active) and re-enters Plan mode so the verdict buttons stay actionable across the round-trip.

> Source: `agent-ui/src/views/message.rs` — `render_plan_review_card`. Verdict dispatch: `agent-ui/src/workspace.rs` — `respond_plan_review` (Implement = retire card + push verdict bubble + `Thread::implement_approved_plan`; ImplementClearContext = archive old thread + spawn fresh thread seeded via `Thread::seed_approved_plan` + attach + run + archive old), `Workspace::send_user_turn` (free-form dismiss = demote card + persist `PlanReview` note). Conversation state: `ConversationState::push_plan_review`, `ConversationState::pop_plan_review_tail` (implement verdict), `ConversationState::consume_plan_review` (free-form dismiss). Shared verdict text: `agent::collaboration_mode::implement_plan_user_message`. Thread seed/run split: `Thread::seed_approved_plan` / `Thread::implement_approved_plan` (`crates/agent/src/thread.rs`). Persisted dismissed-plan note: `agent::db::UiNoteKind::PlanReview` (`record_ui_note` on dismiss, `note_to_item` → `ConvItem::PlanReview { active: false }` on rebuild). Switch round-trip: `Workspace::attach_thread` (stash active plan on switch-away, restore + re-enter Plan on switch-back).

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

Panel body (the card's content, content height — no internal scroll surface, though the plan section has its own bounded scroll region). The conversation-info rows (title, status, changes, branch) sit above the usage tree; the plan section and context budget render from cockpit state owned by the rail.

Contents, top to bottom:

- **Header**: bold title (i18n `context-rail-title`) + a [ContextRailCollapseBtn](#contextrailcollapsebtn) ghost button.
- **Status block** (`cockpit_status_block`): a two-line card — phase label (semibold) on line 1, an xs muted elapsed+tokens meta line (i18n `cockpit-run-status-meta`) on line 2. Elapsed refreshes per-second via the thinking ticker.
- **Agents tree**: [ContextRailAgents](#contextrailagents), with `Main` as the root and every direct or nested sub-agent underneath it.
- **Context budget row**: `context_budget_pct` reads the thread's effective context fill — `agent::compact::effective_context_tokens(thread.messages(), thread.request_token_usage())`, the same max(provider-reported usage, local bytes/4 estimate) the auto-compaction trigger uses, so the display and the trigger agree — against the model window and the `cockpit_auto_compact_threshold` cached on the rail. Renders one line with `pct%` remaining + explicit `used / cap` token counts (i18n `cockpit-context-remaining-ctx`); warning-colored within 10% of the trigger. Hidden entirely when no model is configured.
- **Plan section** (`render_plan_section`, collapsible via `ToggleCockpitTasks` / ctrl/cmd-shift-m, `cockpit_hide_tasks`): the model's execution plan, taken verbatim from the `PlanSnapshot` it publishes via the `UpdatePlan` tool — NOT inferred from the approved `<proposed_plan>` Markdown (that produced a stale dump of implementation bullets). Each step's status (`pending` / `in_progress` / `completed`) is the model's own report; nothing here auto-promotes or infers progress. The header carries a `done/total` count (i18n `cockpit-plan-progress`) and a chevron — those are the only collapse affordance (no hint text). Collapsed shows just the current step (first `in_progress`, else first `pending`) plus a `+N to do` remaining count (i18n `cockpit-plan-remaining`), or an "All done" note (i18n `cockpit-plan-all-done`) when every step is completed. Expanded lists every step in a bounded `max_h(160px).overflow_y_scroll()` region; each row truncates its one-line title with a full-text tooltip. The first snapshot for a thread auto-collapses when it has more than 5 steps (`plan_seen` guards this so later updates preserve the user's collapse choice); the plan is recovered on reload/thread-switch by `agent::plan::rebuild_from_messages` (the latest non-errored `UpdatePlan` tool call in history). Hidden entirely when there is no plan.
- **Changes row**: [ContextRailChangesRow](#contextrailchangesrow).
- **Branch row**: [ContextRailBranchRow](#contextrailbranchrow).
- **Usage section** (`render_usage_section`): `MemoryStick` icon + "Usage" header, then per-model blocks (sorted by total tokens desc; empty for unused models). Each block is:
  - Model id line (truncated to `ENV_MODEL_ID_MAX` chars) + trailing `cache {pct}%` hit-rate badge (i18n `workspace-env-cache-hit-rate`) computed by `cockpit::cache_read_ratio` (denominator = uncached input + cache-read).
  - A two-row tree: `├── 穿透` (i18n `workspace-env-throughput`) carrying `↑{input}` / `↓{output}` animated counters, and `└── 缓存` (i18n `workspace-env-cache`) carrying `↑{cache_create}` / `↓{cache_read}`. The throughput / cache split keeps the cache-read share legible as a branch rather than buried among flat rows. Tree prefixes (`├── ` / `└── `) are painted at `muted.opacity(0.55)` so they read as chrome.
- **Hairline divider**.
- **Sources section**: `Sources` label + "No sources yet" placeholder (ε track).

Each numeric cell animates scoreboard-style (`counter_animated`): a fresh `gen` is appended to the animation id on every value delta, so gpui fires a 600ms `ease_out_quint` tween from the previous rendered value to the new one. `env_counter_state: HashMap<String, (u64, u64)>` lives on `ContextRail`, rebuilt every render inside `render_usage_section` to auto-prune cells whose model disappeared.

> Source: `agent-ui/src/views/context_rail.rs` (`render_panel`)

#### ContextRailAgents

Compact navigation tree headed by `Agents`. `Main` is the root row and reflects the current main thread state; direct and nested sub-agents are indented according to the parent tool-use id recorded by `Workspace`. Every sub-agent row shows the same spinner/terminal status language as [AgentTaskCard](#agenttaskcard), displays `type · short title` with single-line truncation and a tooltip, and opens or focuses its [SubagentPanel](#subagentpanel) when clicked. Completed nodes remain visible for the lifetime of the current main task, including nodes recursively recovered from persisted Agent result envelopes.

> Source: `agent-ui/src/views/context_rail.rs`, `agent-ui/src/workspace.rs`

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

Vertical flex, right panel. A tab container holding the markdown editor, team-member observers, sub-agent observers, browser tabs, and plan preview as peer tab types. Visible while `right_tabs` is non-empty; the active tab's content fills the body.

> Source: `agent-ui/src/workspace.rs`

#### RightTabBar

Top-level underline tab bar over `right_tabs`: `[Editor] [member:plan] [Explore · title] [browser:url] [plan] …`. Selecting a tab switches `active_right_tab`. Member, Subagent, Browser, and PlanPreview tabs carry a `×` suffix that closes the tab (click stops propagation so it does not also select). A sub-agent id can appear only once: a repeat open focuses the existing tab. The Editor tab has no close affordance — it keeps its keyboard toggle (`ToggleEditor` / `CloseEditor`). Switching the main task closes all sub-agent tabs and binds the Agents tree to the newly active task.

> Source: `agent-ui/src/workspace.rs`

#### EditorWriteTab

Plain-text multi-line [InputField](#inputfield) for markdown editing. A second-level Write/Preview toggle lives inside the Editor tab's content area.

> Source: `agent-ui/src/workspace.rs`

#### EditorPreviewTab

Rendered markdown view (`Markdown`).

> Source: `agent-ui/src/workspace.rs`

#### MemberTab

A right-pane tab observing one team worker member. Content is the [MemberPanel](#memberpanel) view.

> Source: `agent-ui/src/workspace.rs`

#### MemberPanel

Read-only observation panel for a single team worker member. Subscribes to the member `Thread`'s events and feeds them into a private [ConversationState](#conversation-state), reusing the full [message](#message) rendering pipeline (agent text, reasoning folds, tool-call cards, peer-message bubbles). Header: member name + status dot (idle/running/gone) + role. A compact task board shows this member's owned tasks plus the unassigned pool, read from the shared team `TaskList`. No composer — the leader is the sole input face.

> Source: `agent-ui/src/views/member_panel.rs`

#### SubagentPanel

Read-only observation panel for one Agent tool invocation (`RightTab::Subagent(tool_use_id)`). A live panel holds a strong child `Entity<Thread>`, subscribes to its `ThreadEvent`s, and applies them to a private [ConversationState](#conversation-state), reusing the main-column `MessageItem` rendering, scrolling, and tail-follow behavior without a composer. Nested `SubagentStarted` events register child nodes back into the current task's `Workspace` registry. Completed live children remain observable until the main task changes. Reloaded tasks recursively scan the Agent ToolUse/ToolResult envelope's stored `messages` and create frozen snapshot panels, requiring no additional database schema.

> Source: `agent-ui/src/views/subagent_panel.rs`, `agent-ui/src/workspace.rs`

#### BrowserView

A right-pane tab hosting an untrusted embedded native webview (`RightTab::Browser(BrowserTabId)`, an equal citizen of the right tab bar alongside `Editor` / `Member`). Chrome row is pure GPUI: back / forward buttons + a single-line address bar whose `Enter` navigates (re-submitting the current URL reloads). The content area is the native `WebViewElement` from `manox-webview`, which tracks the gpui layout via `set_bounds`. Built with `TrustMode::Untrusted`: only the closed-enum notify bridge and the inbound-write request bridge are injected — the page has no Tauri command surface. Tabs are opened via the `OpenBrowserTab` action (`cmd-b`) and closed via the tab's × affordance or `CloseBrowserTab` (`cmd-shift-b`, closes the active browser tab). `tab_id`s are process-unique and woven into the webview label so the host can route inbound notifications back to their tab.

Two transient banners render between the chrome row and the content area, both driven by flags the `BrowserHost` sets on the view (cleared on navigation / resolution):

- **Yield banner** — shown while a `web_explore_yield` call is parked. A "Done" button resolves the parked Task via `WorkspaceBrowserHost::resolve_handback` (the page-side `user_handback` notify is ignored by design — an untrusted page must not resume a parked yield). Retired by the "Done" click, by navigation, or by Stop/Error cleanup (`clear_yields_for_thread`).
- **Read hint** — a muted one-liner shown after `read_text` / `read_dom` / `screenshot` / `eval_script` extracts content from an `https://` origin, signalling that logged-in page content was exposed to the agent.

> Source: `agent-ui/src/views/browser_view.rs`

#### PlanPreviewTab

A right-pane tab (`RightTab::PlanPreview`, an equal citizen of the right tab bar alongside `Editor` / `Member` / `Browser`) that renders the current proposed plan at full height for side-by-side reading while the conversation continues. Opened from a [PlanReviewCard](#planreviewcard)'s "open-in-side-panel" icon (`open_plan_in_editor`) — if one already exists it is focused and its text updated. Content reuses the editor pane's Write/Preview `TabBar` layout with Preview selected (Write is a visual peer, greyed/non-editable) and a `div().overflow_y_scroll().track_scroll(&editor_preview_scroll)` body hosting a `Markdown` view of `plan_preview_text`. Shares the editor-preview `ScrollHandle` so the offset is preserved across tab switches.

> Source: `agent-ui/src/workspace.rs` — `RightTab::PlanPreview`, `Workspace::open_plan_in_editor`, `plan_preview_text`.

#### InboundWriteOverlay

A scrim + card modal mirroring the [ApprovalOverlay](#approvaloverlay), surfaced by `ThreadEvent::InboundAuthorization` when a built-in browser tab calls `window.__manox_request_write__`. Unlike outbound tool approval this axis is `ApprovalMode`-blind — a web page must never gain a write path because the agent runs in Yolo — so the overlay always shows and resolves through `Thread::respond_inbound`, not the outbound approval pipeline (`pending_auths` / `resolve_auth`). Stacked in `pending_inbounds` (LIFO); queued behind any open outbound approval overlay so only one modal shows at a time. Cleared on terminal `Stop` / `Error`.

> Source: `agent-ui/src/workspace.rs`

#### TeamChip

`👥 team · N` accent pill in the composer chip row, shown only while the leader has formed a team. `N` is the worker count (leader excluded). Click toggles the [TeamDrawer](#teamdrawer).

> Source: `agent-ui/src/workspace.rs`

#### TeamDrawer

Popover above the composer: a thin roster of worker members (name / role / status dot / task count). Clicking a row opens (or focuses) that member's [MemberTab](#membertab) in the right pane and closes the drawer. The leader is not listed — it is the main conversation.

> Source: `agent-ui/src/workspace.rs`

---

## 4. ViewMode::Settings

Full-window settings overlay. Slides in from left (180ms), slides out to right (200ms).

#### ManagementBackControl

Unified "back to app" control — `ArrowLeft` + label row mirroring `sidebar::menu_item` density (px_2/py_1p5/gap_2, accent hover wash, `theme.radius`). Mounted as the first row of the [SettingsLeftNav](#settingsleftnav) (above the search input and group list) so the back affordance reads as a peer of the sidebar menu items, not an isolated button. The settings page no longer ships a shared management TitleBar — each management surface reuses the app-page scaffold (sidebar + overlay TitleBar in the main column), and the back control lives in the sidebar.

> Source: `agent-ui/src/views/management_shell.rs`

#### SettingsView

Root of settings overlay, `size_full`, `bg:background`. Mirrors the app-page scaffold: an `h_flex` of `[SettingsLeftNav][main column]`, where the main column is a relative `v_flex` with an absolute [SettingsTitleBar](#settingstitlebar) overlay on top and [SettingsRightPane](#settingsrightpane) content below `pt(TITLE_BAR_HEIGHT)`.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsTitleBar

Absolute-positioned `TitleBar` overlay (`h(TITLE_BAR_HEIGHT)`, `top_0/left_0/right_0`) in the settings main column — same chrome as the conversation column's TitleBar. Shows the currently-selected settings item's localized label (truncating) on the leading side; falls back to the generic "Settings" title when nothing is selected. Carries the window-drag region and macOS traffic-light avoidance. No back button — back lives in [SettingsLeftNav](#settingsleftnav).

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsLeftNav

260px sidebar (`bg:background`, right border) mirroring the app [Sidebar](#sidebar): no standalone TitleBar, the macOS traffic-light buttons float over its transparent top (`pt(top_inset)`, 28px on macOS / 8px elsewhere). Body is a scrollable `v_flex` (`overflow_y_scroll`) holding, top to bottom: the [ManagementBackControl](#managementbackcontrol) ("Back to app" → emits `SettingsEvent::Exit` → returns to [ViewMode::Workspace](#viewmodeworkspace-layout)), the [SettingsSearchInput](#settingssearchinput), and the [SettingsGroupList](#settingsgrouplist).

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsSearchInput

Search/filter input in left nav.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsGroupList

Scrollable list of settings groups with section headers.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsGroup

A labeled group of settings items. Groups: General, Integrations, Coding, Archived.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsItem

Single settings row: icon + label, clickable, highlights when selected.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsRightPane

Right content area, dispatches to panel renderers (and to the [PluginManagerView](#pluginmanagerview) when the Integrations → Plugins item is selected). Each panel/content view owns its own scroll and padding.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsPanel

A specific settings panel rendered in the right pane. Panels: General, Appearance, Config, Personalization, MCP, Environment, Keyboard, Snapshots, Browser, Computer, Hooks, Connections, Git, Worktrees, Archived, Chat Settings.

> Source: `agent-ui/src/views/settings/panels.rs`

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

Plugin/skill/MCP management, rendered as the right-pane content of [ViewMode::Settings](#viewmodesettings) when the Integrations → Plugins item is selected — no longer a standalone full-window mode. The settings shell owns the window TitleBar (drag region + traffic-light avoidance) and the back affordance; this view fills the right pane with its tab bar + tab content.

#### PluginManagerView

View rendered inside [SettingsRightPane](#settingsrightpane) when `settings-item-plugins` is selected.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerTabBar

Four tabs: Marketplace, Plugin, Skill, MCP. The filter search input sits on the trailing side of this tab bar row (moved out of the former management TitleBar, which no longer exists).

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerNoticeBanner

Conditional notice banner.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerBusyIndicator

Conditional busy spinner.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerTabContent

Content area dispatching on selected tab.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### MarketplaceTab

URL input + add btn, marketplace cards (360px), installed plugin cards.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginTab

Installed plugin cards with update/uninstall buttons.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### SkillTab

Skill form (name, description, body) + skill cards with edit/delete.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### McpTab

MCP form (name, command, args, url) + user/plugin MCP server cards.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginCard

Rounded card with bg/border, selected state, action buttons.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### SkillCard

Skill record card with edit/delete buttons.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### McpServerCard

MCP server config card with edit/delete buttons.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### FormCard

Form container with title and input children.

> Source: `agent-ui/src/views/plugin_manager.rs`

---

## 6. ViewMode::Terminal

Full-window terminal emulator.

#### TerminalView

Root view, `size_full`. Owns the focus handle; `focus(&self, window, cx)` is called after spawning/attaching/switching an external-agent session so the TUI receives keystrokes immediately. The focused root intercepts `tab` / `shift-tab` (when no search overlay is open) and forwards them to the PTY as `\t` / `\x1b[Z` with `stop_propagation`, so tab never escapes into GPUI focus traversal. Mouse-wheel events are forwarded to the PTY as xterm mouse reports when a TUI app captures the mouse (e.g. claude code / vim / htop), so its own viewport scrolls; otherwise the local scrollback scrolls.

> Source: `terminal-ui/src/terminal_view.rs`

#### TerminalTabBar

Tab bar for multiple terminal tabs.

> Source: `terminal-ui/src/terminal_view.rs`

#### TerminalGrid

Monospace grid renderer, `flex_1`.

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

First-party selectable text panel (`manox-components::markdown::TerminalPanel`, an `Entity` + `Render` in the `markdown` module) that renders tool output as a terminal-styled shell — **not** a real terminal (no PTY, no grid; `crates/terminal`/`TerminalView` are not involved). One persistent `Entity<TerminalPanel>` is owned per `ToolCallItem` (live + reloaded history) so the document-level `DocSelection` and its `FocusHandle` survive across re-renders: a drag started on frame N keeps its anchor on frame N+1, and Cmd/Ctrl+C reaches a stable focus handle — the same persistence fix that makes assistant/thinking text selectable. The panel renders **only the body**: a transparent, untinted vertical flex (no background fill on the content — it blends into the message list; mono font at `text_sm`, the same size as the thinking body; `px_3 py_2`; `cursor_text`) that mounts a zero-size sentinel first, then a single `RichText` document composed of a prompt block + the body. The prompt block appears **only for `bash`** — the one tool that runs a real shell command a human would type in a terminal; internal tools (`grep`/`read_file`/`edit_file`/`glob`/`list_directory`/`monitor`/…) and MCP tools are manox abstractions, not terminal commands, so they render the body only (no cwd / `❯` preamble that would imply "run this in a shell"). The `bash` prompt block: line 1 cwd (home `~`-collapsed) + `git:{branch}` + status markers (`*mod ✘del !conflict ?untracked`, zero counts and the whole git segment omitted when not a repo); line 2 `❯` (green) + the echoed command. **Three-way text styling** separates prompt chrome / input / output by color × slant: guidance (cwd / `git:` / branch / markers / `❯`) — foreground + **upright**; the echoed command — foreground + **italic**; the body (output) — muted + **italic**. The doc div's `.italic()` sets the base the `RichText` unstyled ranges inherit (so command output / file content / diff context all read muted + italic); the prompt-block guidance and command runs pin their own slant via `styled(color, italic)` so the body's inherited italic does not leak into the prompt. The body is rendered per a `PanelKind` chosen by the agent-ui layer from the tool name (`tool_panel_body`): `File` (`read_file`/`write_file`) — a sequential line-number gutter (the agent-ui layer pre-strips the hashline `[path#TAG]` header + `N:` prefixes for `read_file` and feeds the written `content` for `write_file`, so the panel just numbers the content lines 1..N); `Diff` (`edit_file`) — `+`/`-` lines green/red, `@@` hunk headers cyan, `[path#TAG]`/`---` separators muted; `Plain` (default, `bash` + everything else) — `vte::ansi::Processor` parses SGR foreground/bold/italic into `HighlightStyle` ranges (control bytes stripped from the plain text, truecolor/256/16-color resolved; background SGR tracked but not painted). The whole document wraps at panel width (long lines wrap, no horizontal blow-out) and is one continuous selection across prompt + output. The terminal chrome — a **titlebar** showing the command summary (`gh issue create` for `bash`, `read_file path` otherwise) + status + disclosure chevron, click-to-toggle the body — lives in the agent-ui header (`render_tool_entry` / `render_tool_call`): the titlebar and this body share one bordered rounded frame so the pair reads as a single terminal window (titlebar gets a `border_b_1` separator only while the body is shown). Selection supports double-click word (a click inside a registered inline-code span selects the whole span), triple-click line, drag-extend, and Cmd/Ctrl+C copy — shared with `Markdown` via `DocSelection`. Git state is snapshotted per `bash` panel by the agent-ui layer via a background `git status --porcelain` + `git rev-parse --abbrev-ref HEAD` probe keyed off the thread cwd (internal tools skip the probe — they render no prompt block). `render_tool_output` returns `item.panel.into_any_element()` when mounted, falling back to a fenced code block otherwise.

**Pagination.** A finalized body renders `PAGE_SIZE` (20) lines at a time; a "load more" affordance below the body (a centered `ChevronDown` + `+N` count, top-bordered, hover-tinted) grows the window by another page via `show_more`, clamped to the total. Streaming bodies render the whole live output (no pagination); on the streaming→finalized transition the cursor resets to the first page so the result opens at the top. The panel has **no internal vertical scroll** — the message-list `message-list` div scrolls the whole panel — so `show_more` never touches a scroll handle: growing the window appends lines below the current viewport without jumping to the tail. The pixel-anchored, tail-following message-list arbitration (recomputed each frame in `on_prepaint`) keeps the viewport at the user's reading position across the growth, so successive "load more" clicks stay anchored to the current line rather than snapping to the end.

> Source: `components/src/markdown/terminal_panel.rs` · wired by `agent-ui/src/views/message.rs` (`tool_panel_body` → `ensure_tool_panel` / `sync_tool_*_panel` / `rebuild_tool_panels`, titlebar frame in `render_tool_entry` / `render_tool_call`) + `agent-ui/src/conversation.rs` (`apply` ToolOutput/ToolResult arms, `rebuild_from_messages`)

#### Markdown

Self-built stateful markdown renderer (`manox-components::markdown::Markdown`, an `Entity` + `Render`) replacing `gpui_component::TextView::markdown`. Owns the source + an `IncrementalParser` (parse-once: freezes the completed prefix so a streaming append only re-parses the growing tail) + a document-level `DocSelection`. The `Render` builds a focusable vertical flex root mounting a zero-size sentinel as the first child — the sentinel clears the per-frame block registry at paint start, then each block's `RichText` re-registers its geometry during paint; the root's mouse listeners hit-test that registry to drive one continuous selection across paragraph / code / list boundaries, and the key listener copies it on Cmd/Ctrl+C. Click semantics: single click places the anchor + starts a drag; double-click selects the word at the click (a click landing inside a registered inline-code span selects the whole span verbatim); triple-click selects the line. `RichText` composes `StyledText` for shaping/glyph-painting and overlays rounded inline-code washes + this block's slice of the shared selection. Block visuals: paragraphs/headings via `StyledText::with_highlights`; code blocks with line-number gutter + `overflow_x_scroll` + tree-sitter highlighting (highlight result cached per `(lang, content_hash)`); unified-diff blocks with accent wash + left bar; GFM tables with column alignment + horizontal scroll; task-list checkboxes; code/diff block hover copy button. Streaming bodies paint plain text + cursor; the full layout mounts once the stream ends.

#### TurnFrame

Shared framed text container (`manox-components::turn_frame::TurnFrame`) used for user turns. It paints one continuous accent-colored stroke path for the door-shaped frame, leaving the bottom center open while preserving rounded `╰─` / `─╯` corners. The lower stroke is lifted slightly into the bottom padding so the open edge visually hugs the final text line without letting markdown content overflow its layout box. The component does not fill the content background, does not rely on masking a complete border, and avoids assembling the frame from independent rail nodes. Callers provide header, trailing controls, and body content.

> Source: `components/src/turn_frame.rs`

#### Icon

Named icon from the icon set (e.g., `IconName::Folder`, `IconName::Search`).

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

#### OnRequest

Green — ask before each tool call (default).

#### AutoReview

Blue — auto-approve safe tools, ask for risky ones.

#### Yolo

Red — approve everything without asking.

---

## 9. Collaboration Modes

The thread's [`ModeKind`](#modechip) (`collaboration_mode` on `Thread`). Two modes, cycled by [ModeChip](#modechip) click / `/plan` / `shift-tab` (the `CycleCollaborationMode` action).

#### Default

Execution mode. Full tool set. No mode-specific developer instructions injected beyond the default instructions.

#### Plan

Read-only research-and-plan mode. Tool set filtered to read-only (no write/bash/submit tools). Per-mode overrides apply (`reasoning_effort = Medium`, plan-mode developer instructions injected as a fixed-position `<collaboration_mode>` User message at request-build time — never persisted into history, never woven into the system prompt, so the provider prefix cache stays warm across mode-stable turns). The model submits a plan by emitting a single `<proposed_plan>…</proposed_plan>` block; block content surfaces as a [PlanReviewCard](#planreviewcard) drawer card in the message list (verdict buttons in the card). Goal accounting is suspended in Plan mode.

---

## 10. Tool Call Statuses

States of a [ToolCallCard](#toolcallcard).

#### PendingApproval

Waiting for user, triggers [ApprovalOverlay](#approvaloverlay).

#### Running

Spinner or shimmer.

#### Success

Green check, collapsible output.

#### Error

Red X, error output.

#### Denied

Greyed out, "denied" label.

---

## 11. Reasoning Effort Levels

Values selectable in [EffortMenu](#effortmenu).

#### Low
#### Medium
#### High
#### XHigh
#### Max
#### Ultracode
#### Auto


