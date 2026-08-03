// Capture the TS Pi differential fixtures from a TS Pi checkout.
//
// This file runs under bun: the real TS sources are imported by absolute
// path from the checkout, so every fixture records the TS implementation's
// actual output. The temp node_modules shims provisioned by
// refresh_ts_pi_fixtures.sh only need to evaluate at import time — the
// captured functions never call through them.
//
// Usage: bun capture-fixtures.ts <ts-pi-repo> <fixture-out-dir>
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const repo = process.argv[2];
const outDir = process.argv[3];
if (!repo || !outDir) {
	console.error("usage: bun capture-fixtures.ts <ts-pi-repo> <fixture-out-dir>");
	process.exit(1);
}
mkdirSync(outDir, { recursive: true });

const agent = join(repo, "packages/agent/src");
const codingAgent = join(repo, "packages/coding-agent/src");

const { substituteArgs } = await import(join(codingAgent, "core/prompt-templates.ts"));
const { formatSkillInvocation } = await import(join(agent, "harness/skills.ts"));
const { serializeConversation } = await import(join(agent, "harness/compaction/utils.ts"));
const { runAgentLoop } = await import(join(agent, "agent-loop.ts"));

// --- substitute-args.txt ---------------------------------------------------
// Case list shared with the Rust differential test: each block records the
// input, the args, and the real TS output. Cases cover defaults, empty args,
// $0, slice bounds, and the single-pass non-recursion contract.
const substituteCases = [
	{
		input: "Run $1 then $2. All: $@. Slice: ${@:2}. Slice3: ${@:1:2}. Args: $ARGUMENTS.",
		args: ["build", "test", "lint"],
	},
	{
		input: "Pos: ${1:-fallback}. Empty: ${2:-fallback}. All: ${@:-fallback}. Args: ${ARGUMENTS:-fallback}.",
		args: ["build"],
	},
	{
		input: "No args: ${1:-x} ${@:-none} ${ARGUMENTS:-none} $1 $@.",
		args: [],
	},
	{
		input: "$0 | ${0:-def} | $1 | ${@:0} | ${@:2:1} | ${@:9}",
		args: ["a", "b"],
	},
	{
		input: "Keep literal args: $1 $2.",
		args: ["$@ and $ARGUMENTS and $1"],
	},
	{
		input: "Slice bounds: ${@:5} | ${@:2:1} | ${@:1:9}.",
		args: ["a", "b", "c"],
	},
];
const substituteBlocks = substituteCases.map((c) => {
	const expected = substituteArgs(c.input, c.args);
	return `input: ${JSON.stringify(c.input)}\nargs: ${JSON.stringify(c.args)}\nexpected: ${JSON.stringify(expected)}`;
});
writeFileSync(join(outDir, "substitute-args.txt"), substituteBlocks.join("\n\n") + "\n");

// --- skill-invocation.txt (+ with-instructions variant) --------------------
const skill = {
	name: "review",
	description: "",
	filePath: "/proj/skills/review.md",
	content: "Check the diff carefully.",
};
writeFileSync(join(outDir, "skill-invocation.txt"), formatSkillInvocation(skill) + "\n");
writeFileSync(
	join(outDir, "skill-invocation-with-instructions.txt"),
	formatSkillInvocation(skill, "Focus on the diff.") + "\n",
);

// --- compaction-serialization.txt ------------------------------------------
// The same LLM-shaped conversation the Rust differential test builds.
const conversation = [
	{ role: "user", content: [{ type: "text", text: "hello" }], timestamp: Date.now() },
	{ role: "assistant", content: [{ type: "text", text: "hi" }], timestamp: Date.now() },
	{
		role: "assistant",
		content: [{ type: "toolCall", name: "read", arguments: { path: "a.rs" }, id: "t1" }],
		timestamp: Date.now(),
	},
	{
		role: "toolResult",
		toolCallId: "t1",
		toolName: "read",
		content: [{ type: "text", text: "ok" }],
		isError: false,
		timestamp: Date.now(),
	},
];
writeFileSync(
	join(outDir, "compaction-serialization.txt"),
	serializeConversation(conversation) + "\n",
);

// --- agent-loop-events.txt --------------------------------------------------
// A plain single-turn run through the real agent loop with a one-shot LLM
// stream: the emitted lifecycle is the stable artifact the Rust loop must
// reproduce.
const events = [];
const emit = async (event) => {
	events.push(event);
};
const finalMsg = {
	role: "assistant",
	content: [{ type: "text", text: "ok" }],
	model: "m",
	provider: "p",
	api: "test",
	stopReason: "stop",
	usage: {
		inputTokens: 0,
		outputTokens: 0,
		cacheReadInputTokens: 0,
		cacheCreationInputTokens: 0,
		cacheWrite1h: undefined,
		reasoningTokens: undefined,
		totalTokens: 0,
		cost: undefined,
	},
	timestamp: Date.now(),
};
const response = {
	async *[Symbol.asyncIterator]() {
		yield { type: "start", partial: { ...finalMsg, content: [] } };
	},
	async result() {
		return finalMsg;
	},
};
const streamFn = async () => response;
const context = {
	systemPrompt: "sys",
	messages: [],
	tools: [],
	model: {
		provider: "p",
		api: "test",
		id: "m",
		contextWindow: 100_000,
		maxTokens: 8_192,
		thinking: "none",
		metadata: {},
	},
	thinkingLevel: undefined,
	cacheRetention: { strategy: "last_breakpoint_only", maxCacheableTokens: 0 },
	sessionId: undefined,
	metadata: {},
};
const config = { model: context.model, convertToLlm: (msgs) => msgs };
await runAgentLoop(
	[{ role: "user", content: [{ type: "text", text: "hi" }], timestamp: Date.now() }],
	context,
	config,
	emit,
	undefined,
	streamFn,
);
const kind = (event) => {
	const t = event.type;
	if (t === "message_start" || t === "message_end") return `${t}(${event.message.role})`;
	return t;
};
const pascal = (t) =>
	t.replace(/_([a-z])/g, (_, c) => c.toUpperCase()).replace(/^[a-z]/, (c) => c.toUpperCase());
writeFileSync(join(outDir, "agent-loop-events.txt"), events.map((e) => pascal(kind(e))).join("\n") + "\n");
