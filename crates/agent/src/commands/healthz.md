---
description: Run a full built-in tool regression check. The LLM exercises every built-in tool group (FS, shell, user interaction, metadata, monitor, web, subagent, goal, plan, worktree, team, browser), collects PASS/FAIL/SKIP per tool, and reports a summary table.
---
You are running a **health check** (`/healthz`) on the manox built-in toolset. Your job is to systematically exercise every built-in tool group, record PASS/FAIL/SKIP for each tool, and produce a final summary table. This is a regression test — run every group even if an earlier one fails.

## Scope

**In scope** — test these tools:

| Group | Tools |
|-------|-------|
| FS Read | Read, List, Grep, Glob |
| FS Write | Write, Edit |
| Shell | Bash, BashOutput |
| User Interaction | AskUserQuestion |
| Metadata | SelfInfo, Skill |
| Monitor | Monitor, TaskStop |
| Web | WebFetch |
| Subagent | Agent |
| Goal | GetGoal, CreateGoal, UpdateGoal |
| Plan | UpdatePlan |
| Worktree | EnterWorktree, ExitWorktree |
| Team | TaskCreate, TaskList, TaskGet, TaskUpdate, SendMessage, TeamCreate, TeamSpawn, TeamDisband |
| Browser | WebExploreOpen, WebExploreNavigate, WebExploreReadText, WebExploreReadDom, WebExploreClick, WebExploreType, WebExploreScroll, WebExploreScreenshot, WebExploreYield, WebExploreClose |

**Out of scope** — do NOT test (skip silently, do not report):

- LSP tools: LspStatus, LspEnsure, LspWaitReady, GoToDefinition, FindReferences, Hover, DocumentSymbols, WorkspaceSymbols, Diagnostics
- Conditional tools: Code, ToolSearch
- External plugin subagents: remora:remora-task
- Plugin commands: gitwork:*

## Execution rules

1. **Continue on failure.** If a tool fails, record FAIL and move to the next tool. Do not abort early. The goal is to surface every broken tool in one run.
2. **Clean up side effects.** Tools with persistent state must be cleaned up after verification:
   - Write/Edit: write to a temp path under the project `target/` dir, verify, then delete.
   - EnterWorktree/ExitWorktree: enter a worktree, verify cwd changed, then exit with `remove` to clean it up.
   - TeamCreate/TeamDisband: create a team, exercise team tools, then disband to clean up.
   - CreateGoal: create a goal, verify, then clear it with `/goal clear` semantics (you cannot call slash commands — instead use UpdateGoal to mark complete, then verify it is gone or completed).
   - Browser: open a tab, exercise read-only tools, then close it with WebExploreClose.
3. **Parallel calls.** When tools in the same group are independent (e.g. Read + List + Grep + Glob), call them in parallel in one turn.
4. **Minimal side effects.** Use the simplest valid input for each tool — `echo healthz` for Bash, `https://example.com` for WebFetch, `about:blank` or a stable URL for WebExploreOpen.
5. **SKIP for conditional tools not in the exclude list.** If a tool is registered but cannot run due to a missing runtime condition (e.g. no browser host), record SKIP with the reason in Notes.
6. **AskUserQuestion**: ask a single confirmation question like "healthz: ready to continue?" with yes/no options. PASS when you receive a response.

## Per-group expectations

### FS Read (parallel)
- **Read**: read `Cargo.toml` in the project root. PASS if content includes `[workspace]` or `[package]`.
- **List**: list `crates/` directory. PASS if it returns directory entries.
- **Grep**: grep for `fn main` in `crates/`. PASS if it returns matches.
- **Glob**: glob `**/*.toml`. PASS if it returns paths.

### FS Write (sequential, clean up after)
- **Write**: write `healthz-smoke-test.txt` under `target/` with content `healthz-write-test`. PASS on success.
- **Edit**: edit that file with a hashline patch (e.g. INS.TAIL appending a line). PASS on success.
- Read the file back to verify content, then delete it.

### Shell
- **Bash**: run `echo healthz`. PASS if output contains `healthz`.
- **BashOutput**: start a background bash `sleep 2 && echo done`, poll with BashOutput until it exits. PASS if `done` is received.

### User Interaction
- **AskUserQuestion**: ask "healthz: ready to continue?" with yes/no. PASS when the user responds.

### Metadata (parallel)
- **SelfInfo**: call SelfInfo. PASS if it returns a non-empty thread id.
- **Skill**: call Skill with any registered skill name (e.g. `gitwork:deliver`). PASS if it returns non-empty content.

### Monitor
- **Monitor**: start a monitor on a short command (e.g. `echo monitor-test`). PASS if it starts and returns a task id.
- **TaskStop**: stop the monitor task. PASS if it stops successfully.

### Web
- **WebFetch**: fetch `https://example.com`. PASS if it returns HTML content.

### Subagent
- **Agent**: spawn an `Explore` subagent with the task "find Cargo.toml in the project root and report its path". PASS if the subagent returns a non-empty conclusion mentioning `Cargo.toml`.

### Goal (sequential, clean up after)
- **CreateGoal**: create a goal with objective "healthz smoke test". PASS on success.
- **GetGoal**: call GetGoal. PASS if it returns the goal you just created.
- **UpdateGoal**: mark the goal complete. PASS on success. Then verify the goal is cleared or completed.

### Plan
- **UpdatePlan**: publish a minimal plan (e.g. one step "healthz smoke test"). PASS on success.

### Worktree (sequential, clean up after)
- **EnterWorktree**: enter a new worktree named `healthz-smoke`. PASS if cwd changes to the worktree path.
- **ExitWorktree**: exit with `remove` to clean up. PASS on success.

### Team (sequential, clean up after)
- **TeamCreate**: create a team named "healthz-test". PASS on success.
- **TeamSpawn**: spawn one member (any available subagent type). PASS on success.
- **TaskCreate**: create a task "healthz team test". PASS if it returns a task id.
- **TaskList**: list tasks. PASS if the task appears.
- **TaskGet**: read the task by id. PASS if it returns the task.
- **TaskUpdate**: update the task status to completed. PASS on success.
- **SendMessage**: send a message to the member (e.g. "healthz ping"). PASS on success.
- **TeamDisband**: disband the team. PASS on success. Clean up.

### Browser (sequential, clean up after)
- **WebExploreOpen**: open a tab to `https://example.com`. PASS if it returns a tab id.
- **WebExploreNavigate**: navigate the tab to `https://example.com`. PASS on success.
- **WebExploreReadText**: read the page text. PASS if content is non-empty.
- **WebExploreReadDom**: read the DOM. PASS if HTML is non-empty.
- **WebExploreScreenshot**: take a viewport snapshot. PASS on success.
- **WebExploreScroll**: scroll down by 100px. PASS on success.
- **WebExploreType**: type into a search input or any focusable element (if available). PASS or SKIP if no focusable element.
- **WebExploreClick**: click a link or element (if available). PASS or SKIP if no clickable element.
- **WebExploreYield**: yield control to the user briefly, then continue. PASS or SKIP if not applicable.
- **WebExploreClose**: close the tab. PASS on success.

## Output format

After running all groups, produce a single Markdown table:

```
| Tool | Group | Result | Notes |
|------|-------|--------|-------|
| Read | FS Read | PASS | |
| Write | FS Write | PASS | |
| ... | ... | ... | ... |
```

Result is one of `PASS`, `FAIL`, `SKIP`. Notes is empty for PASS, the error message for FAIL, or the skip reason for SKIP.

End with a summary line:

```
**Summary: X passed, Y failed, Z skipped.**
```
