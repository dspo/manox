# Context optimization offline replay

Run the fixed acceptance corpus with:

```sh
cargo run -p agent --example context_replay
```

The command exits non-zero unless all of these gates pass:

- median estimated raw input reduction is at least 50%;
- aggregate model-call regression is at most 5%;
- median end-to-end proxy regression is at most 5%;
- the canonical messages remain byte-for-byte unchanged;
- every retained tool use still has exactly one matching tool result.

The corpus at `crates/agent/tests/fixtures/context_replay.json` is fixed,
versioned, and contains generated/sanitized workload shapes: repeated reads,
grep refinement, noisy build logs, an edit/read refresh barrier, image history,
and hybrid Code batching. It contains no user transcripts.

`baseline_input_tokens` and `optimized_input_tokens` use deterministic UTF-8
bytes/4, the same local estimator used by compaction. They are suitable for a
stable offline regression gate, not a claim about a provider's exact tokenizer.
The prompt-specific DeepSeek V3 tokenizer check remains a separate gate.

No provider is called during replay. The E2E field is therefore labelled a
proxy: fixed recorded model latency plus the locally measured projection time.
Provider-backed latency and quality canarying belongs in an opt-in online job;
it must not make the offline CI gate flaky or require credentials.
