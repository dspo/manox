# Pi porting policy and audit map

## Product decisions

- Make `crates/pi` the eventual sole manox harness kernel.
- Remove the existing self-developed manox harness after migration.
- Do not preserve a long-term dual stack, adapter, or compatibility promise
  for the old harness.
- Keep crate pi independent before it is mature. Do not require early manox
  wiring.
- Use TypeScript Pi core behavior and runnable crate pi examples as the
  pre-migration acceptance target.

## Accepted implementation differences

Do not report these as drift by themselves:

- differences required or strongly encouraged by Rust language semantics;
- different internal implementations of same-purpose tools;
- tool schema differences when purpose and normal usage remain broadly
  equivalent;
- in-process versus subprocess implementations;
- Rust-specific safety, ownership, trait, or cancellation structure;
- extra functionality that does not break Pi behavior.

Report the difference when it changes observable ordering, persistence,
resume behavior, cancellation, errors, supported input kinds, or the ability
to complete a normal Pi workflow.

## Scope lenses

Use both lenses when they differ:

1. **Agreed migration scope** — honor exclusions in the current plan, such as
   UI, dynamic extension loading, or broad provider SDK coverage.
2. **Full package parity** — describe how much of `pi-agent-core`, `pi-ai`, and
   non-UI `pi-coding-agent` exists, without presenting excluded breadth as an
   immediate blocker.

Never let an intentional exclusion make the full-package estimate appear
complete.

## Source map

Start with these TypeScript areas:

- `packages/agent/src/agent-loop.ts`
- `packages/agent/src/agent.ts`
- `packages/agent/src/types.ts`
- `packages/agent/src/harness/agent-harness.ts`
- `packages/agent/src/harness/session/`
- `packages/agent/src/harness/compaction/`
- `packages/ai/src/types.ts`
- `packages/ai/src/api/`
- `packages/ai/src/providers/`
- `packages/ai/src/utils/`
- `packages/coding-agent/src/core/agent-session.ts`
- `packages/coding-agent/src/core/agent-session-runtime.ts`
- `packages/coding-agent/src/core/sdk.ts`
- `packages/coding-agent/src/core/session-manager.ts`
- `packages/coding-agent/src/core/model-*.ts`
- `packages/coding-agent/src/core/resource-loader.ts`
- `packages/coding-agent/src/core/settings-manager.ts`
- `packages/coding-agent/src/core/trust-manager.ts`
- `packages/coding-agent/src/core/compaction/`
- `packages/coding-agent/src/core/tools/`

Compare primarily with:

- `crates/pi/src/agent_loop.rs`
- `crates/pi/src/agent.rs`
- `crates/pi/src/harness.rs`
- `crates/pi/src/types.rs`
- `crates/pi/src/tool.rs`
- `crates/pi/src/session/`
- `crates/pi/src/compaction/`
- `crates/pi/src/provider/`
- `crates/pi/src/tools/`
- `crates/pi/src/settings.rs`
- `crates/pi/src/trust.rs`
- `crates/pi/src/cache_stats.rs`
- `crates/pi/examples/`
- `crates/pi/tests/`

Discover paths again before concluding that a capability was removed; Pi
upstream reorganizes files over time.

## Capability rubric

Judge a capability through these levels:

1. **Shape only** — type/enum/schema exists.
2. **Local behavior** — isolated function works.
3. **Integrated behavior** — public path invokes it.
4. **Durable behavior** — session persistence and restore preserve it.
5. **Operational behavior** — cancellation, retry, errors, examples and
   regressions work.

Report levels 1–2 as partial, not complete.

## Drift taxonomy

- **Missing:** TS behavior has no effective Rust path.
- **Wrong:** Rust path exists but produces incompatible observable behavior.
- **Extra:** Rust-only behavior or subsystem; assess risk but do not subtract
  parity unless it interferes.
- **Intentional:** Explicitly accepted difference or excluded scope.
- **Uncertain:** Evidence is insufficient; state the exact check needed.

For severity:

- **P0:** data loss, security boundary failure, or broadly unusable kernel.
- **P1:** normal core workflow or required acceptance example is broken.
- **P2:** important edge case, incomplete resume/configuration, or misleading
  completion claim.
- **P3:** hygiene, test isolation, or low-impact divergence.

## Upstream baseline record

When asked to maintain tracking state, add or update a compact section in
`crates/pi/PLAN.md` unless the repository already has a dedicated ledger:

```markdown
## TS Pi upstream baseline

- Source: `<absolute or canonical repository identifier>`
- Commit: `<full SHA>`
- Reviewed: `<YYYY-MM-DD>`
- Scope: `packages/agent`, `packages/ai`,
  `packages/coding-agent/src/core`
- Applicable deferred deltas:
  - `<capability and evidence>`
- Validation: `<commands and results>`
```

Advance the commit only after all listed package deltas have been classified.
Keep unresolved applicable work visible in the normal pending-work section.
