# Goal: 消息区基石重建(Rock A + markdown 完全重建,一次性施工)

> load .claude/skills/gpui and .claude/skills/gpui-component before you do

## 目标(一句话)

放弃 `gpui::list` 虚拟列表与现有 per-block markdown renderer,以**像素锚定的 `ScrollHandle` 普通滚动容器 + first-party `Entity<Markdown>`(stateful、parse-once 缓存、文档级 `Selection`)**为基石,从零重建消息区渲染,根治"消息莫名其妙滚上天"与"拷贝任意文本失效"两个顽疾,**不破坏任何已打磨的 UI 视觉语言**。

## 根因(施工者必读,不得回归到此)

`gpui::list` 的滚动锚定模型是 `ListOffset { item_ix, offset_in_item }`——按「item 索引 + 进入该 item 的像素偏移」锚定,**不是按「绝对像素位置」锚定**(见 `crates/agent_ui/.../gpui/src/elements/list.rs` 的 `ScrollAnchor::Absolute|Proportional`)。manox 与 zed 用同一个 gpui rev,此模型一致。

在流式 LLM 输出下,index-anchor 必然失准,触发点全是架构性的、补丁修不动:

1. 流式增长:底部 item 每帧变高,`scroll_to_end`/tail-follow 与 remeasure 竞态。
2. **宽度变化**(最经典的"滚上天"):窗口/侧栏 resize → markdown 全体重排 → 所有 item 高度同时变 → `offset_in_item` 失去意义。补丁层根本碰不到。
3. streaming→finalized body 切换:Stop 翻 streaming flag,正文从增量布局切全量布局,高度跳一帧。
4. splice 与布局帧之间的 count 竞竞态。

现有 `workspace.rs` 里 `apply_list_outcome` / `remeasure_items` / `Absolute anchor` / `splice preserve scroll` 整套机器,都是在 index-anchor 这个脆弱基石之上糊的补偿层。**禁止再在它上面打补丁。** 整层换掉。

zed 用同一个 `gpui::list`,靠 per-chunk `track_scroll` + collaboration `workspace.follow` + 异步 `ParsedMarkdown` 预布局绕过脆弱性。manox 是单用户本地工作台,不需要 collab follow 语义,因此**直接换更简单的基石**,不抄 zed 的上层补丁。

## 冻结的决策(已与用户确认,不再讨论方向)

- **Rock A**:像素锚 `ScrollHandle` + `Entity<Markdown>` 缓存 + 文档级 Selection,**放弃 `gpui::list`**。
- **完全重建 markdown crate**:新建 first-party `Entity<Markdown>`,替换现有 `crates/components/src/markdown/`,**不改造旧 renderer**。
- **一次性完成**(不分阶段),在本 worktree `rooted-quarry` 施工。
- 长线程不虚拟化起步;真到上千条再按需加「视口外 item 只占位不渲染」的轻量虚拟化,**且该虚拟化必须建在像素锚之上,不得引入 index-anchor**。

## 基石规格(Rock A)

### 滚动容器

- 消息列改为单个 `div().overflow_y_scroll().track_scroll(&scroll_handle)`(或等价 GPUI 滚动容器),**像素锚定**:滚动位置 = 绝对像素偏移。
- item 在视口下方增长、宽度变化导致全量重排、body 切换——**这些都不再移动视口**(像素位置绝对)。这是"滚上天"从根上消失的机制,不是糊住的,是不存在了。
- tail-follow(贴底跟随)由 `scroll_handle` 层实现:流式追加时,若用户处于贴底态则 `scroll_to_bottom`,否则保持像素位置不动(用户向上翻阅时不被拽回)。
- click-to-reveal(点 outline tick 跳到某 turn):基于该 turn 对应元素的 `bounds`(或记录的像素偏移)跳转,不再依赖 `list_state.bounds_for_item`。

### 消息 item

- 每个 message / 每个 markdown 正文块持有一个 `Entity<Markdown>`(stateful)。parse 一次,缓存 `ParsedMarkdown` 结构与 layout 结果;流式追加只增量追加 source + 重布局受影响块,**不每帧全量重 parse**。
- 删除现有 `workspace.rs` 里所有 `list_state` / `splice` / `remeasure_items` / `ApplyOutcome` / `ListAlignment::Bottom` / `MSG_LIST_OVERDRAW` 相关补偿代码。`ConversationState` 增量构建 `ConvItem` 列表的逻辑可保留(它只是数据层,不是渲染锚),但渲染侧改挂 `ScrollHandle`。

## Markdown crate 规格

新建/重建 `crates/components/src/markdown/`(`mod.rs` + 拆分的子模块),对外提供:

- `Markdown`:`Entity`,`new(id, source)` + `append(&str)` / `replace(source)` / `reset(source)` / `source()` / `is_parsing()` / `parsed()`。
- **文档级 `Selection`**(核心):单一 `Selection` 跨整篇 markdown,基于 source-string index。鼠标拖选 → 坐标 → source index 区间 → 跨段落/代码块高亮 + `Cmd/Ctrl+C` 复制选中文本。**这是"拷贝任意文本"的落点。**
- source-index ↔ glyph 位置的双向映射,供 `Selection` 查询(段落/代码块各自实现 `hit(source_index) -> pixel` 与 `pixel -> source_index`)。
- 异步解析:`Markdown` 在 background parse,layout 阶段用缓存结果;**必须解决 manox 旧版"同步是为了让 list 高度缓存诚实"的顾虑——像素锚下高度缓存诚实与否不再影响滚动锚**(像素位置绝对),故可安全异步。
- 高亮复用现有 gpui-component 语法高亮链路(不重造 tree-sitter 集成)。
- 保留现有视觉:行内 code wash、code/diff 块 hover copy button、heading mode、mermaid(若现有有)等。视觉语言不变,只换底层。

### 选择实现要求

- `selectable()` 不再是 per-block no-op,改为向文档级 `Selection` 注册本块的可选文本范围,由文档级 Selection 统一驱动高亮与复制。
- 跨块拖选必须连续(从段落拖进代码块、跨多个段落)。

## 必须保留(已打磨 UI,禁止破坏视觉语言)

- 消息卡片层次、assistant/user/reasoning/tool-call/notice 的视觉区分。
- Reasoning 折叠、ToolCall/AgentTask 卡片、code block copy button。
- composer、`+`/`⁄` 弹出菜单、settings、plugin_manager、title_menu、outline、sidebar。
- 字体:body Lilex Light,markdown bold/headings Medium,italic 切面。
- i18n:模型面向字符串一律英文不本地化;UI chrome 经 `agent::i18n::t`。新代码遵守同一边界。

渲染层重写后,上层 views(`message.rs` 等)改为消费新的 `Entity<Markdown>` + 新滚动容器,卡片/折叠/工具卡的视觉**保持像素级一致**(允许内部实现变,不允许外观变)。

## 施工顺序(自主执行,逐项打勾;允许在项内自由细化)

1. **测绘**:读全 `crates/components/src/markdown/{mod,ast,incremental,rich_text,theme}.rs`、`crates/agent-ui/src/views/message.rs`、`crates/agent-ui/src/conversation.rs`、`workspace.rs` 的消息列渲染与 `apply_list_outcome`/splice/remeasure 全部站点,列清依赖面。
2. **markdown crate 骨架**:`Entity<Markdown>` + 异步 parse + 缓存 `ParsedMarkdown`,单测覆盖 parse/append/replace。
3. **文档级 Selection**:source-index 模型 + 跨块高亮 + Cmd/Ctrl+C,单测覆盖 hit/选区/复制。
4. **消息列换基石**:`ScrollHandle` 像素锚容器替换 `gpui::list`;重写 tail-follow、click-to-reveal;删除 list 补偿层。
5. **上层 views 迁移**:`message.rs` 等改挂新 `Entity<Markdown>`,视觉对齐旧版。
6. **`ConversationState` 适配**:确认数据层与渲染解耦,`apply`/`rebuild_from_messages` 不再依赖 list 语义。
7. **收尾**:删旧 markdown 文件、删死代码、更新 `UI-MAP.md`、跑全量验证。

## 不变量与项目纪律(违反即未完成)

- **零构建告警**:`cargo clippy --all-targets -- -D warnings` 全绿,`cargo build` 无 warning。禁止裸丢弃 `Result`,禁止新增 `#[allow]`(除非 lint 与既有设计冲突且英文注释说明)。
- **前缀缓存**:不得碰 `build_completion_request` 与消息组装管线。本任务只动 UI 渲染层。
- **禁止抄袭 zed 代码**:可借鉴架构思想,禁止复制粘贴 zed `markdown`/`selection.rs` 代码后修改。**禁止依赖 zed 的 `markdown` crate**(依赖墙 `language`/`settings`/`theme`/`ui`/`sum_tree`/`stacksafe` + GPL)。
- **英文注释,面向终态(不变量/意图),非必要不注释**;禁止过程流水账、禁止标注来源出处。
- **不保留兼容层**:不写 `v0`/`legacy_`/`backward_compat`,字段失去理由直接删,删代码同步删测试。
- **单二进制、单进程**;crate 依赖只认 crates.io 或 `git = "..."`,workspace 内部成员间 `path` 例外。
- 运行时禁止 schema migration(本任务不涉 db,若涉及则只对开发者本机手动改)。

## 完成条件(全部满足才算 done)

- [ ] 消息列以像素锚 `ScrollHandle` 渲染,代码中无 `gpui::list` / `ListState` / `ListAlignment` / `ListOffset` / `ListSizingBehavior` 残留(消息列路径)。
- [ ] 流式输出、窗口/侧栏 resize、streaming→finalized 切换三种场景下,视口不再"滚上天"(用户向上翻阅时尤其不被拽回)。
- [ ] 跨段落 + 代码块拖选连续,Cmd/Ctrl+C 复制选中任意文本可用。
- [ ] `cargo clippy --all-targets -- -D warnings` 绿,`cargo build` 无 warning,`cargo test` 通过(含新增 markdown/selection 单测)。
- [ ] UI 视觉与旧版一致(卡片/折叠/字体/配色),`UI-MAP.md` 同步更新。
- [ ] 旧 `crates/components/src/markdown` per-block renderer 代码已删,无死代码。

## 自主执行约束

- 中途不停下来问许可;遇设计岔路按"最简、不破坏上述不变量、视觉一致"原则自行裁决并继续。
- 仅在以下情形停下回报:① 全部完成条件达成;② 触发上述不变量无法绕过的硬冲突;③ 发现本 spec 与代码现实矛盾且无法自行澄清。
