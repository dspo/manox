---
description: Run a full built-in tool regression check. The LLM exercises every built-in tool group (FS, shell, user interaction, metadata, monitor, web, subagent, goal, plan, worktree, team, browser), collects PASS/FAIL/SKIP results, and reports a summary table.
---
You are running a **health check** (`/healthz`) on the manox built-in toolset. Your job is to systematically exercise every built-in tool group, record PASS/FAIL/SKIP for each tool, and produce a final summary table. This is a regression test — run every group even if an earlier one fails.

## Critical: project-agnostic

This test MUST NOT assume any particular project layout. Do NOT read project-specific files like `Cargo.toml`, `package.json`, or `crates/`. Instead, create all test fixtures yourself in the system temp directory. Use `Bash` with `mktemp -d` to get a temp directory, then create fixture files there. All test inputs are self-contained.

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
| Team | TeamCreate, TeamSpawn, TaskCreate, TaskList, TaskGet, TaskUpdate, SendMessage, TeamDisband |
| Browser | WebExploreOpen, WebExploreNavigate, WebExploreReadText, WebExploreReadDom, WebExploreClick, WebExploreType, WebExploreScroll, WebExploreScreenshot, WebExploreYield, WebExploreClose |

**Out of scope** — do NOT test (skip silently, do not report):

- LSP tools: LspStatus, LspEnsure, LspWaitReady, GoToDefinition, FindReferences, Hover, DocumentSymbols, WorkspaceSymbols, Diagnostics
- Conditional tools: Code, ToolSearch
- External plugin subagents: remora:remora-task
- Plugin commands: gitwork:*

## Execution rules

1. **Continue on failure.** If a tool fails, record FAIL and move to the next tool. Do not abort early.
2. **Self-contained fixtures.** Create all test files in a temp directory (via `Bash` → `mktemp -d`). Never read project-specific files.
3. **Clean up side effects.** Tools with persistent state must be cleaned up after verification:
   - Write/Edit: write to the temp dir, verify, then delete.
   - EnterWorktree/ExitWorktree: enter a worktree, verify cwd changed, then exit with `remove` to clean it up.
   - TeamCreate/TeamDisband: create a team, exercise team tools (including member tool execution), then disband.
   - CreateGoal: create a goal, verify, then clear it via UpdateGoal(complete) and verify.
   - Browser: open a tab, exercise read-only tools, then close it with WebExploreClose.
4. **Parallel calls.** When tools in the same group are independent (e.g. Read + List + Grep + Glob), call them in parallel in one turn.
5. **SKIP for unavailable tools.** If a tool is registered but cannot run due to a missing runtime condition (e.g. no browser host), record SKIP with the reason in Notes.
6. **AskUserQuestion**: ask a single confirmation question "healthz: ready to continue?" with yes/no options. PASS when you receive a response.

## Test cases

### Group: Fixture setup

Run `mktemp -d` via Bash to create a temp dir. Inside it, create:
- `hello.txt` — content: `healthz line 1\nhealthz line 2\nhealthz line 3`
- `subdir/` — an empty subdirectory
- `config.json` — content: `{"name": "healthz", "version": 1}`

This temp dir is the base for all FS tests. Clean it up at the end.

### Group: FS Read (parallel, use temp dir fixtures)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 1 | **Read** | Read `<tmpdir>/hello.txt` | Content contains `healthz line 1` |
| 2 | **List** | List `<tmpdir>` | Returns `hello.txt`, `subdir`, `config.json` |
| 3 | **Grep** | Grep `healthz` in `<tmpdir>` | Returns matches in `hello.txt` |
| 4 | **Glob** | Glob `**/*.json` rooted at `<tmpdir>` | Returns path to `config.json` |

### Group: FS Write (sequential, use temp dir)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 5 | **Write** | Write `<tmpdir>/writable.txt` with content `healthz write test` | Success, no error |
| 6 | **Edit** | Edit `<tmpdir>/writable.txt` — use the hashline patch format: write `[<absolute_path>#<tag>]` header on line 1, then `INS.TAIL:` on line 2, then `+appended by edit` as the new content line. The `#<tag>` is any 4-hex-digit placeholder (e.g. `#A000`). | Success, no error |
| — | (verify) | Read `<tmpdir>/writable.txt` back | Content contains both `healthz write test` and `appended by edit` |
| — | (cleanup) | Delete `<tmpdir>/writable.txt` | — |

### Group: Shell

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 7 | **Bash** | Run `echo healthz` | Output contains `healthz` |
| 8 | **BashOutput** | Start background bash `sleep 1 && echo bg-done`, poll with BashOutput until process exits | Receives `bg-done` in output |

### Group: User Interaction

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 9 | **AskUserQuestion** | Ask "healthz: ready to continue?" with options Yes/No | Receives a response |

### Group: Metadata (parallel)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 10 | **SelfInfo** | Call SelfInfo | Returns non-empty output containing a thread id |
| 11 | **Skill** | Call Skill with any registered skill name you know (e.g. `gitwork:deliver`). If you do not know any skill name, call Skill with name `gitwork:deliver` anyway — PASS if it returns skill content, SKIP if it returns an error indicating no skills are registered. | Returns non-empty content (skill body), or SKIP with reason |

### Group: Monitor

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 12 | **Monitor** | Start a monitor on command `echo monitor-test` | Returns a task id |
| 13 | **TaskStop** | Stop the monitor task | Stops successfully, no error |

### Group: Web

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 14 | **WebFetch** | Fetch `https://example.com` | Returns HTML content containing `Example Domain` |

### Group: Subagent

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 15 | **Agent** | Spawn an `Explore` subagent with the task: "Read the file at `<tmpdir>/hello.txt` and report its contents." | The subagent's final message contains `healthz line 1` — proving it correctly used the Read tool within its restricted tool set |

### Group: Goal (sequential, clean up after)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 16 | **CreateGoal** | Create a goal with objective "healthz smoke test" | Success, no error |
| 17 | **GetGoal** | Call GetGoal | Returns the goal with objective "healthz smoke test" |
| 18 | **UpdateGoal** | Mark the goal complete | Success, no error |

### Group: Plan

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 19 | **UpdatePlan** | Publish a plan with one step: "healthz smoke test" (status completed) | Success, no error |

### Group: Worktree (sequential, clean up after)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 20 | **EnterWorktree** | Enter a new worktree named `healthz-smoke` | cwd changes to a path under `.claude/worktrees/` |
| 21 | **ExitWorktree** | Exit with `action: remove` | Success, cwd returns to original, worktree cleaned up |

### Group: Team (sequential, clean up after)

This group tests both the team coordination tools AND that a spawned member can execute tool calls within its sub-agent context.

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 22 | **TeamCreate** | Create a team named "healthz-test" | Success, no error |
| 23 | **TeamSpawn** | Spawn one member with subagent_type `Explore`, role "tester", name "tester-1" | Success, no error |
| 24 | **TaskCreate** | Create a task: "Read the file at `<tmpdir>/hello.txt` and report its contents." | Returns a task id (e.g. T1) |
| 25 | **TaskList** | List tasks on the team board | The task appears in the list |
| 26 | **TaskGet** | Read the task by id | Returns the task with correct subject |
| 27 | **TaskUpdate** | Update the task status to `in_progress` | Success, no error |
| 28 | **SendMessage** | Send a message to the member "tester-1": "Please use the Read tool to read `<tmpdir>/hello.txt` and reply with the contents." | Success, no error |
| — | (verify) | Wait for the member to process the message and reply. Poll by calling SendMessage to yourself or by sending another message asking for status. In practice, the member's reply arrives as a user-role message in your conversation — continue your turn and check if the reply containing `healthz line 1` arrives. If no reply after 2-3 attempts, record FAIL for SendMessage with "no member reply" in Notes. | Member's reply contains `healthz line 1` — proving the team member could execute tool calls and report results back |
| 29 | **TeamDisband** | Disband the team | Success, no error |

### Group: Browser (sequential, clean up after)

| # | Tool | Action | PASS criterion |
|---|------|--------|-----------------|
| 30 | **WebExploreOpen** | Open a tab to `https://example.com` | Returns a tab id |
| 31 | **WebExploreNavigate** | Navigate the tab to `https://example.com` | Success, no error |
| 32 | **WebExploreReadText** | Read the page text | Content is non-empty and contains `Example Domain` |
| 33 | **WebExploreReadDom** | Read the DOM HTML | HTML is non-empty and contains `<html` |
| 34 | **WebExploreScreenshot** | Take a viewport snapshot | Success, returns DOM structure |
| 35 | **WebExploreScroll** | Scroll down by 100px | Success, no error |
| 36 | **WebExploreType** | Type `healthz` into any focusable element (e.g. search input if available) | PASS or SKIP if no focusable element |
| 37 | **WebExploreClick** | Click on a link or element | PASS or SKIP if no clickable element |
| 38 | **WebExploreYield** | Yield control to the user, then continue after they resume | PASS or SKIP if not applicable |
| 39 | **WebExploreClose** | Close the tab | Success, no error |

### Group: Fixture cleanup

Delete the temp directory created in fixture setup.

## Output format

After running all groups, produce a single Markdown table:

```
| # | Tool | Group | Result | Notes |
|---|------|-------|--------|-------|
| 1 | Read | FS Read | PASS | |
| 2 | List | FS Read | PASS | |
| ...
```

Result is one of `PASS`, `FAIL`, `SKIP`. Notes is empty for PASS, the error message for FAIL, or the skip reason for SKIP.

End with a summary line:

```
**Summary: X passed, Y failed, Z skipped.**
```
