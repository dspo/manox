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
const { prepareBranchEntries } = await import(
	join(agent, "harness/compaction/branch-summarization.ts"),
);
const { convertToLlm } = await import(join(agent, "harness/messages.ts"));
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

// --- branch-summary-preparation.txt -----------------------------------------
// The fixed abandoned branch the Rust differential test mirrors: messages
// (user/assistant/toolResult), a custom message, a harness-authored branch
// summary (seeding file lists), and a compaction carrier. `prepareBranchEntries`
// runs under the real TS implementation; the fixture records the selected
// roles, the accumulated file lists, and the serialized conversation.
const branchEntries = [
	{
		type: "message",
		id: "u1",
		parentId: null,
		timestamp: 1,
		message: { role: "user", content: [{ type: "text", text: "first" }], timestamp: 1 },
	},
	{
		type: "message",
		id: "a1",
		parentId: "u1",
		timestamp: 2,
		message: {
			role: "assistant",
			content: [{ type: "text", text: "hi" }],
			model: "m",
			provider: "p",
			api: "test",
			stopReason: "stop",
			usage: {},
			timestamp: 2,
		},
	},
	{
		type: "message",
		id: "a2",
		parentId: "a1",
		timestamp: 3,
		message: {
			role: "assistant",
			content: [{ type: "toolCall", name: "write", arguments: { path: "a.rs" }, id: "t1" }],
			model: "m",
			provider: "p",
			api: "test",
			stopReason: "toolUse",
			usage: {},
			timestamp: 3,
		},
	},
	{
		type: "message",
		id: "a3",
		parentId: "a2",
		timestamp: 4,
		message: {
			role: "assistant",
			content: [{ type: "toolCall", name: "read", arguments: { path: "b.rs" }, id: "t2" }],
			model: "m",
			provider: "p",
			api: "test",
			stopReason: "toolUse",
			usage: {},
			timestamp: 4,
		},
	},
	{
		type: "message",
		id: "tr",
		parentId: "a3",
		timestamp: 5,
		message: {
			role: "toolResult",
			toolCallId: "t1",
			toolName: "write",
			content: [{ type: "text", text: "ok" }],
			isError: false,
			timestamp: 5,
		},
	},
	{
		type: "custom_message",
		id: "c1",
		parentId: "tr",
		timestamp: 6,
		customType: "note",
		content: [{ type: "text", text: "custom note" }],
		display: true,
		details: undefined,
	},
	{
		type: "branch_summary",
		id: "bs1",
		parentId: "c1",
		timestamp: 7,
		fromId: "u1",
		summary: "prior branch",
		details: { readFiles: ["old.rs"], modifiedFiles: ["old.rs"] },
		usage: undefined,
		fromHook: false,
	},
	{
		type: "compaction",
		id: "cp1",
		parentId: "bs1",
		timestamp: 8,
		summary: "prior compaction",
		tokensBefore: 100,
		details: undefined,
		usage: undefined,
		fromHook: false,
		firstKeptEntryId: null,
		retainedTail: undefined,
	},
];
const branchPrep = prepareBranchEntries(branchEntries, 0);
const llmMessages = convertToLlm(branchPrep.messages);
const branchConversation = serializeConversation(llmMessages);
const modified = new Set([...branchPrep.fileOps.edited, ...branchPrep.fileOps.written]);
const branchReadFiles = [...branchPrep.fileOps.read].filter((f) => !modified.has(f)).sort();
const branchModifiedFiles = [...modified].sort();
const branchRole = (m) => {
	if (m.role === "user") return "user";
	if (m.role === "assistant") return "assistant";
	if (m.role === "custom") return "custom";
	if (m.role === "branchSummary") return "branchSummary";
	if (m.role === "compactionSummary") return "compactionSummary";
	return m.role;
};
writeFileSync(
	join(outDir, "branch-summary-preparation.txt"),
	`roles: ${JSON.stringify(branchPrep.messages.map(branchRole))}\n` +
		`readFiles: ${JSON.stringify(branchReadFiles)}\n` +
		`modifiedFiles: ${JSON.stringify(branchModifiedFiles)}\n` +
		`conversation:\n${branchConversation}\n`,
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
