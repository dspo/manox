---
name: port-pi
description: Port and continuously track the non-UI core of the local TypeScript Pi repository into the Rust crate pi. Use when reviewing parity, investigating drift, syncing and triaging upstream Pi changes, implementing a parity slice, updating the migration plan or upstream baseline, or validating pi-agent-core, pi-ai, pi-coding-agent core, compaction, sessions, providers, tools, hooks, and examples.
---

# Port Pi

Treat the TypeScript Pi checkout as the behavioral source and `crates/pi` as
the Rust destination. Preserve observable semantics while using idiomatic Rust.

Read [references/parity-policy.md](references/parity-policy.md) before judging
scope, classifying drift, or updating progress.

## Establish the comparison

1. Resolve the destination from the current repository:

   ```sh
   git rev-parse --show-toplevel
   git status --short --branch
   git rev-parse HEAD
   ```

2. Resolve the TypeScript source in this order:
   - a path explicitly supplied by the user;
   - `PI_TS_REPO`;
   - `~/projects/github/pi`.

3. Read repository instructions and `crates/pi/PLAN.md`. Treat newer user
   decisions as authoritative when the plan is stale.

4. Verify both repositories are clean enough to inspect. Preserve unrelated
   changes. Fetch the relevant remotes when the task requires the latest
   upstream state, then report the exact source and destination SHAs. Never
   pull, reset, rebase, or switch a dirty worktree merely to perform a review.

5. Find the last recorded upstream baseline:

   ```sh
   rg -n "upstream|baseline|基线|packages/agent|packages/ai" crates/pi/PLAN.md docs .agents
   ```

   If no reliable baseline exists, perform a current-state capability audit
   and say that the result is a new baseline rather than inventing one.

## Select the workflow

### Review current parity

Inspect code on both sides; do not infer parity from filenames, TODOs, test
counts, or plan checkboxes.

1. Build a capability matrix for:
   - `packages/agent` ↔ loop, Agent, Harness, session and compaction;
   - `packages/ai` ↔ message/model types, streaming protocols and provider
     transformation;
   - `packages/coding-agent/src/core` ↔ AgentSession/runtime, model and
     credential resolution, resources, settings/trust, session operations,
     retry/compaction orchestration, and non-UI tools.
2. Trace each important behavior end to end:

   ```text
   public API → state/config → loop/provider/tool → event reduction
              → persistence → restore/resume
   ```

3. Classify every difference as `missing`, `wrong`, `extra`, `intentional`,
   or `uncertain`. Apply the acceptance rules from the reference before
   calling something drift.
4. Prioritize observable failures and broken vertical paths over surface-area
   counts. Give concrete file/line evidence from both repositories.
5. Separate:
   - selected-scope parity, such as the provider protocols intentionally
     implemented now;
   - full-package parity, such as every API/provider supported by `pi-ai`.

### Track an upstream iteration

Compare the recorded baseline with current TypeScript Pi:

```sh
git -C "$PI_TS_REPO" log --oneline --decorate <baseline>..HEAD -- \
  packages/agent packages/ai packages/coding-agent/src/core
git -C "$PI_TS_REPO" diff --stat <baseline>..HEAD -- \
  packages/agent packages/ai packages/coding-agent/src/core
git -C "$PI_TS_REPO" diff --name-status <baseline>..HEAD -- \
  packages/agent packages/ai packages/coding-agent/src/core
```

Read the changed implementations and tests. Group upstream changes by
behavioral capability, not by commit count. For each group decide:

- already equivalent in Rust;
- applicable and still missing;
- invalidates an existing Rust assumption;
- intentionally out of scope;
- documentation/test-only.

Update the repository's upstream baseline only after the comparison is
complete. Record the exact TS SHA, reviewed package paths, date, applicable
unported deltas, and validation status. Do not erase older deferred work merely
because the baseline advanced.

### Implement a parity slice

1. Choose the smallest complete behavior, including its public entry point,
   state transition, event/persistence effects, restore behavior, and tests.
2. Read the TypeScript implementation and its tests before designing the Rust
   change.
3. Translate semantics rather than syntax:
   - use Rust ownership, enums, traits, cancellation, and async patterns;
   - keep ordering, terminal states, retry boundaries, and durable state
     equivalent where consumers can observe them.
4. Prefer differential or table-driven tests derived from the same fixtures.
   Add a regression test for every discovered incorrect behavior.
5. Keep `PLAN.md` factual. Mark a feature complete only when the normal public
   path uses it; parsing a type or adding an uncalled helper is not completion.
6. Do not wire manox to crate pi before maturity unless the user explicitly
   asks. Do not add compatibility with the old manox harness.

## Audit high-risk seams

Always inspect these seams when relevant:

- lifecycle event ownership, ordering, backpressure and terminal messages;
- steering/follow-up queue modes and turn preparation callbacks;
- parallel tool execution, progress timing, cancellation and termination;
- provider error classification, retry, overflow → compact → retry;
- safe compaction boundaries, split-turn, retained tails and repeated
  compaction;
- JSONL parent/leaf integrity, entry projection, model/thinking/active-tools
  restoration and branch movement;
- settings override presence versus default values;
- model/runtime/credential/resource resolution in coding-agent;
- examples that exercise a full public path rather than only compiling.

## Validate

Run validation proportionate to the change. For a full crate review, run:

```sh
cargo fmt -p pi -- --check
cargo clippy -p pi --all-targets -- -D warnings
cargo check -p pi --examples
env -u ALL_PROXY -u all_proxy -u HTTP_PROXY -u http_proxy \
  -u HTTPS_PROXY -u https_proxy cargo test -p pi --all-targets
cargo run -p pi --example agent_loop_tool
cargo run -p pi --example compact_run
cargo run -p pi --example session_roundtrip
```

Do not count `cargo check --examples` as evidence that examples run. Run
networked examples only when credentials and network access are intentionally
available.

For semantic ports, also compare the smallest stable artifacts available:
event traces, final messages, JSONL entries, restored context, compaction
preparations, provider payloads, or error classifications.

## Report

Lead with whether the current slice is usable and whether any acceptance
command fails. Then provide:

1. actionable findings ordered by severity;
2. fixed findings from the previous baseline;
3. remaining capability gaps by package;
4. intentional and acceptable differences;
5. extra/free-form Rust work that does not count toward parity;
6. source/destination SHAs and exact validation results;
7. a capability-weighted completion range, explicitly labeled as an estimate.

For implementation tasks, summarize changed files and tests. For review-only
tasks, do not modify product code or publish comments unless asked.
