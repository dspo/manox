# UI Map

Shared vocabulary for every named UI component in manox. When discussing UI, reference
component names from this file so both parties refer to the same thing.

Component names use PascalCase. The hierarchy mirrors the visual containment tree.

---

## 索引

### 顶层

- [Window](#window) · [Workspace](#workspace) · [ViewMode](#viewmode) · [ViewMode::Workspace](#viewmodeworkspace-layout) · [ViewMode::Settings](#viewmodesettings) · [ViewMode::Plugins](#viewmodeplugins) · [ViewMode::Terminal](#viewmodeterminal)

### Sidebar

- [Sidebar](#sidebar) · [SidebarScrollBody](#sidebarscrollbody) · [SidebarMenuSection](#sidebarmenusection) · [SidebarMenuItem](#sidebarmenuitem) · [SidebarProjectsSection](#sidebarprojectssection) · [SidebarProjectGroup](#sidebarprojectgroup) · [SidebarConversationsSection](#sidebarconversationssection) · [SidebarThreadItem](#sidebarhreaditem) · [SidebarDivider](#sidebardivider)

### MainColumn

- [MainColumn](#maincolumn) · [TitleBar](#titlebar) · [TitleBarThreadTitle](#titlebarthreadtitle) · [TitleBarGoalChip](#titlebargoalchip) · [TitleBarMenuButton](#titlebarmenubutton) · [Body](#body)

### Hero

- [Hero](#hero)

### MessageArea

- [MessageArea](#messagearea) · [OutlineRail](#outlinerail) · [OutlineTick](#outlinetick) · [OutlineHoverCard](#outlinehovercard) · [MessageList](#messagelist) · [MessageItem](#messageitem)

### MessageItem 变体

- [UserMessage](#usermessage) · [AssistantMessage](#assistantmessage) · [ReasoningBlock](#reasoningblock) · [ToolCallCard](#toolcallcard) · [AgentTaskCard](#agenttaskcard) · [ErrorMessage](#errormessage) · [NoticeMessage](#noticemessage) · [RecapCard](#recapcard) · [RetryBadge](#retrybadge)

### Footer / Composer

- [Footer](#footer) · [Composer](#composer) · [ComposerDivider](#composerdivider) · [AttachmentChips](#attachmentchips) · [AttachmentChip](#attachmentchip) · [ComposerInputRow](#composerinputrow) · [InputField](#inputfield) · [SendBtn](#sendbtn) · [ModelChip](#modelchip) · [AccessChip](#accesschip) · [EffortChip](#effortchip) · [ProjectChip](#projectchip) · [PlusBtn](#plusbtn) · [TeamChip](#teamchip)

### AskDrawer

- [AskDrawer](#askdrawer) · [AskDrawerHeader](#askdrawerheader) · [AskDrawerQuestion](#askdrawerquestion) · [AskDrawerOptions](#askdraweroptions) · [AskDrawerOtherInput](#askdrawerotherinput) · [AskDrawerResponseInput](#askdrawerresponseinput) · [AskDrawerNav](#askdrawernav)

### Popups & Dropdowns

- [PlusMenu](#plusmenu) · [CompletionPopover](#completionpopover) · [ModelMenu](#modelmenu) · [AccessMenu](#accessmenu) · [EffortMenu](#effortmenu) · [ProjectMenu](#projectmenu) · [TitleMenu](#titlemenu) · [GoalPopover](#goalpopover)

### Overlays

- [ApprovalOverlay](#approvaloverlay) · [PlanApprovalOverlay](#planapprovaloverlay) · [BlankProjectOverlay](#blankprojectoverlay)

### EditorPane

- [EditorDivider](#editordivider) · [RightPane](#rightpane) · [RightTabBar](#righttabbar) · [EditorWriteTab](#editorwritetab) · [EditorPreviewTab](#editorpreviewtab) · [MemberTab](#membertab) · [MemberPanel](#memberpanel)

### Composer (team)

- [TeamChip](#teamchip) · [TeamDrawer](#teamdrawer)

### Settings

- [SettingsView](#settingsview) · [SettingsTitleBar](#settingstitlebar) · [SettingsLeftNav](#settingsleftnav) · [SettingsSearchInput](#settingssearchinput) · [SettingsGroupList](#settingsgrouplist) · [SettingsGroup](#settingsgroup) · [SettingsItem](#settingsitem) · [SettingsRightPane](#settingsrightpane) · [SettingsPanel](#settingspanel) · [SettingsSectionCard](#settingssectioncard) · [SettingsRow](#settingsrow) · [SettingsSectionHeader](#settingssectionheader) · [SettingsHairline](#settingshairline)

### PluginManager

- [PluginManagerView](#pluginmanagerview) · [PluginManagerHeader](#pluginmanagerheader) · [PluginManagerTabBar](#pluginmanagertabbar) · [PluginManagerNoticeBanner](#pluginmanagernoticebanner) · [PluginManagerBusyIndicator](#pluginmanagerbusyindicator) · [PluginManagerTabContent](#pluginmanagertabcontent) · [MarketplaceTab](#marketplacetab) · [PluginTab](#plugintab) · [SkillTab](#skilltab) · [McpTab](#mcptab) · [PluginCard](#plugincard) · [SkillCard](#skillcard) · [McpServerCard](#mcpservercard) · [FormCard](#formcard)

### Terminal

- [TerminalView](#terminalview) · [TerminalTabBar](#terminaltabbar) · [TerminalGrid](#terminalgrid)

### Shared Primitives

- [Button](#shared-primitives) · [Input](#shared-primitives) · [PopupMenu](#shared-primitives) · [PopupMenuItem](#shared-primitives) · [TabBar](#shared-primitives) · [Tag](#shared-primitives) · [Markdown](#markdown) · [TurnFrame](#turnframe) · [Icon](#shared-primitives) · [ScrollHandle](#shared-primitives) · [TitleBar](#shared-primitives) · [ContextMenu](#shared-primitives) · [Tooltip](#shared-primitives)

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
Full-window settings overlay with slide-in animation.

#### ViewMode::Plugins
Full-window plugin/skill/MCP manager.

#### ViewMode::Terminal
Full-window terminal emulator.

---

## 3. ViewMode::Workspace Layout

The default mode. Three horizontal slots: Sidebar, MainColumn, EditorPane.

```
┌──────────┬──┬─────────────────────────┬──┬──────────────┐
│          │  │                         │  │              │
│ Sidebar  │▌│     MainColumn           │▌│ EditorPane   │
│          │  │                         │  │ (conditional)│
│          │  │                         │  │              │
└──────────┴──┴─────────────────────────┴──┴──────────────┘
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

Top section: New Thread, Search, Scheduled, Plugins menu items.

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

Bottom section: loose (non-project) threads.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarThreadItem

Single thread row: title, short-id tag (shimmer if running), token count, time, archive btn. Hover/active/selected wash uses the thread's last saved approval-mode color.

> Source: `agent-ui/src/views/sidebar.rs`

#### SidebarDivider

6px drag handle between Sidebar and MainColumn, `cursor:col-resize`.

> Source: `agent-ui/src/workspace.rs`

### 3.2 MainColumn

Central area, flex-1, relative positioning.

#### MainColumn

Vertical flex container, fills remaining width.

> Source: `agent-ui/src/workspace.rs`

#### TitleBar

Absolute-positioned top bar, height `TITLE_BAR_HEIGHT`, contains thread title and "..." menu.

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

Horizontal flex: [OutlineRail](#outlinerail) + [MessageList](#messagelist).

> Source: `agent-ui/src/workspace.rs`

#### OutlineRail

40px gutter, vertical flex, centered ticks (one per user turn).

> Source: `agent-ui/src/workspace.rs` + `views/outline.rs`

#### OutlineTick

16×2px rounded bar per user turn, color varies by state.

> Source: `agent-ui/src/views/outline.rs`

#### OutlineHoverCard

260px popover card on tick hover, shows message preview.

> Source: `agent-ui/src/views/outline.rs`

#### MessageList

Virtualized variable-height `list` (`gpui::list` + `ListState`); only viewport + overdraw items lay out. Tail-follow via `FollowMode::Tail`; count/height reconciled via `splice`/`remeasure_items` from the `ThreadEvent` handler.

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

Collapsible: chevron + "Reasoning" label + left-bordered muted body.

> Source: `agent-ui/src/views/message.rs`

#### ToolCallCard

Collapsible card: title + status badge + 220px output (monospace or markdown).

Statuses: `PendingApproval` | `Running` | `Success` | `Error` | `Denied` — see [ToolCallStatus](#tool-call-statuses).

> Source: `agent-ui/src/views/message.rs`

#### AgentTaskCard

Expandable sub-agent card: title + status + collapsed tail / expanded nested conversation.

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

Trigger: [PlusBtn](#plusbtn). "Add" menu: files, goal, plan mode, plugins.

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

#### PlanApprovalOverlay

Trigger: `exit_plan_mode` called. Similar modal: plan text + Approve / Reject buttons.

> Source: `agent-ui/src/workspace.rs`

#### BlankProjectOverlay

Trigger: "Create blank project" from [ProjectMenu](#projectmenu). Centered modal: project name input + confirm.

> Source: `agent-ui/src/workspace.rs`

### 3.3 EditorPane

Right-side panel, shown when `editor_open` is true. 640px default (320–960 draggable).

#### EditorDivider

6px drag handle between MainColumn and the right pane (conditional — shown while any right-pane tab is open).

> Source: `agent-ui/src/workspace.rs`

#### RightPane

Vertical flex, right panel. A tab container holding one Editor slot (the markdown scratchpad) plus one slot per team worker member. Visible while `right_tabs` is non-empty; the active tab's content fills the body.

> Source: `agent-ui/src/workspace.rs`

#### RightTabBar

Top-level underline tab bar over `right_tabs`: `[Editor] [member:plan] [member:expl] …`. Selecting a tab switches `active_right_tab`. Member tabs carry a `×` suffix that closes the tab (click stops propagation so it does not also select). The Editor tab has no close affordance — it keeps its keyboard toggle (`ToggleEditor` / `CloseEditor`).

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

#### TeamChip

`👥 team · N` accent pill in the composer chip row, shown only while the leader has formed a team. `N` is the worker count (leader excluded). Click toggles the [TeamDrawer](#teamdrawer).

> Source: `agent-ui/src/workspace.rs`

#### TeamDrawer

Popover above the composer: a thin roster of worker members (name / role / status dot / task count). Clicking a row opens (or focuses) that member's [MemberTab](#membertab) in the right pane and closes the drawer. The leader is not listed — it is the main conversation.

> Source: `agent-ui/src/workspace.rs`

---

## 4. ViewMode::Settings

Full-window settings overlay. Slides in from left (180ms), slides out to right (200ms).

#### SettingsView

Root of settings overlay, `size_full`, `bg:background`.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsTitleBar

Top bar with back button.

> Source: `agent-ui/src/views/settings/mod.rs`

#### SettingsLeftNav

260px sidebar: back btn + search input + scrollable group list.

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

Right content area, `overflow_y_scroll`, `p_4`, dispatches to panel renderers.

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

## 5. ViewMode::Plugins

Full-window plugin/skill/MCP management view.

#### PluginManagerView

Root view, `size_full`.

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerHeader

Top bar: back btn + title + search input (280px).

> Source: `agent-ui/src/views/plugin_manager.rs`

#### PluginManagerTabBar

Four tabs: Marketplace, Plugin, Skill, MCP.

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

Root view, `size_full`.

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

#### Markdown

Self-built markdown renderer (`manox-components::markdown::Markdown`) replacing `gpui_component::TextView::markdown`. Per-block layout: paragraphs/headings via `StyledText::with_highlights`; code blocks with line-number gutter + `overflow_x_scroll` + tree-sitter highlighting; unified-diff blocks with accent wash + left bar; GFM tables with column alignment + horizontal scroll; task-list checkboxes. Streaming bodies paint plain text + cursor and mount the full layout once the stream ends. Cross-block selection is a follow-up; per-block copy buttons remain.

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

## 9. Tool Call Statuses

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

## 10. Reasoning Effort Levels

Values selectable in [EffortMenu](#effortmenu).

#### Low
#### Medium
#### High
#### XHigh
#### Max
#### Ultracode
#### Auto
