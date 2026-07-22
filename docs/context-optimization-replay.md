# Context optimization offline replay

Run the fixed acceptance corpus with:

```sh
cargo run -p agent --example context_replay
```

The command exits non-zero unless all of these gates pass:

- median estimated raw input reduction is at least 50%;
- aggregate model-call regression is at most 5%;
- compaction count does not regress;
- median end-to-end proxy regression is at most 5%;
- the canonical messages remain byte-for-byte unchanged;
- every retained tool use still has exactly one matching tool result.

The v2 corpus at `crates/agent/tests/fixtures/context_replay.json` is fixed,
versioned, and contains generated/sanitized baseline/optimized traces: repeated reads,
grep refinement, noisy build logs, an edit/read refresh barrier, image history,
and hybrid Code batching. It contains no user transcripts.

Each trace event produces the cumulative request that the next model turn
would see. `baseline_input_tokens` and `optimized_input_tokens` are medians of
those per-turn requests, using deterministic UTF-8 bytes/4 over production
retention/rewrite/image projection and schemas generated from the real tool
registry. Model-call counts come from trace turns plus threshold-derived
compaction calls; they are not score fields stored in the fixture.

The estimator is suitable for a stable offline regression gate, not a claim
about a provider's exact tokenizer.
The prompt-specific DeepSeek V3 tokenizer check remains a separate gate.

No provider is called during replay. The E2E field is therefore labelled a
proxy: trace-derived model/compaction calls multiplied by fixed recorded model
latency, plus the locally measured production projection time.
Provider-backed latency and quality canarying belongs in an opt-in online job;
it must not make the offline CI gate flaky or require credentials.
