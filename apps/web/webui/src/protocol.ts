// Typed wire protocol — the single source of truth is the ts-rs bindings
// generated from the Rust `manox-protocol` crate
// (crates/manox-protocol/bindings/protocol.ts). This module re-exports those
// types so the rest of the webui imports one hub; UI-side projections of the
// remaining opaque `JsonValue` payloads (plan / goal / background-task /
// subagent-child stream / §E.3 conversation-info fold) live below as they
// are not part of the Rust protocol. The v1 aggregate `ThreadInfoSnapshot`
// (+ its `UsageBreakdown` / `SubagentSnapshot` / `GitStats` members) was
// deleted at T10c with the §D.6 notes it summarized; the §E.3
// `GetConversationInfo` fold (`ConversationInfo`) is its successor.

export * from '../../../../crates/manox-protocol/bindings/protocol';

// ── UI-side projections of opaque JsonValue payloads ───────────────────────
// The Rust `ServerNote` carries several `JsonValue` fields whose shapes the
// client casts into these typed projections. They are UI concerns (not a wire
// contract the server guarantees structurally), so they stay here rather than
// in the protocol crate.

/** How the user resolves a submitted plan review. */
export type PlanVerdictChoice = 'execute_keep' | 'execute_compact' | 'refine';

/** User-side Goal lifecycle action (mirrors the gpui host's `/goal` verbs). */
export type GoalAction = 'create' | 'edit' | 'replace' | 'clear' | 'pause' | 'resume';

/** One selectable choice inside an `AskUserQuestion` question. */
export interface AskOptionWire {
	label: string;
	description?: string;
	recommended?: boolean;
}

/** One question step of an `AskUserQuestion` input payload. */
export interface AskQuestionWire {
	question: string;
	header?: string;
	multiSelect?: boolean;
	options: AskOptionWire[];
}

/** One block inside a `model_chat` message (bare-model completion). */
export type ModelChatBlock =
	| { type: 'text'; text: string }
	| { type: 'thinking'; text: string }
	| { type: 'tool_call'; id: string; name: string; input: unknown }
	| { type: 'tool_result'; id: string; content: string; isError?: boolean }
	| { type: 'image'; data: string; mimeType: string };

/** One conversation message relayed to the model via `model_chat`. */
export interface ModelChatMessage {
	role: 'system' | 'user' | 'assistant';
	content: ModelChatBlock[];
}

/** A tool definition relayed to the model; execution stays in VS Code. */
export interface ModelChatTool {
	name: string;
	description: string;
	inputSchema: Record<string, unknown>;
}

/** Wire vocabulary emitted by the actor (agent::ToolCallStatus, kebab-case).
 * The webview store folds terminal values into UI semantics
 * (success → completed, error → failed); the rest pass through. */
export type ToolCallStatus =
	| 'pending-approval'
	| 'running'
	| 'success'
	| 'continued'
	| 'error'
	| 'denied'
	| 'cancelled'
	| (string & {});

/** Tool-authorization policy: mirrors the actor's `agent::PermissionMode`
 * kebab wire values. `read-only` refuses fs mutations, `workspace-write`
 * confines writes to the workspace and state home, `danger-full-access`
 * runs every tool call without confinement. */
export type ApprovalMode = 'read-only' | 'workspace-write' | 'danger-full-access';

/** User-facing reasoning-effort knob for the model dropdown (`high`/`max`). */
export type ReasoningEffort = 'high' | 'max';

/** One slash-completion entry: a built-in/prompt-macro command or a skill. */
export interface CommandEntry {
	name: string;
	/** Null for built-ins; the webview translates them via `i18n_key`. */
	description: string | null;
	kind: 'command' | 'skill';
	argument_hint: string | null;
	/** Fluent key (agent locales) for built-in commands; the webview's own
	 * i18n dict carries the copy. Null for markdown commands and skills. */
	i18n_key?: string | null;
}

/** Serde wire form of agent plan snapshots. */
export interface PlanStepWire {
	step: string;
	status: 'pending' | 'in_progress' | 'completed';
}

export interface PlanSnapshotWire {
	explanation: string | null;
	steps: PlanStepWire[];
}

/** Serde wire form of the thread's persistent Goal (agent::goal::ThreadGoal). */
export interface GoalSnapshotWire {
	thread_id: string;
	goal_id: string;
	objective: string;
	/** serde snake_case wire form of GoalStatus. */
	status: 'active' | 'paused' | 'blocked' | 'budget_limited' | 'complete';
	token_budget: number | null;
	tokens_used: number;
	time_used_seconds: number;
	status_reason: string | null;
	created_at: number;
	updated_at: number;
}

// ── §E.3 Q-face: the `GetConversationInfo` response payload ────────────────
// The server-side journal fold (conversation-info plugin mode) replaces the
// doomed GetUsage / UsageSnapshot / ThreadInfo request-note path. Field
// names mirror the Rust `fold_conversation_info` json keys (camelCase).

/** One per-model aggregate row of the §E.3 fold (`models[]`).
 * `contextWindow` / `hitRate` / `pct` are token-meter semantics the server
 * fills in from the provider registry; `null` while unavailable. */
export interface ConversationModelRow {
	provider: string;
	/** Canonical wire identity (L8): `{provider}/{model}`. */
	model: string;
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	reasoning: number;
	calls: number;
	/** Last request's full context numerator. */
	lastTotal: number;
	contextWindow: number | null;
	hitRate: number | null;
	pct: number | null;
}

/** Working-tree stats inside the §E.3 fold (`git`; null placeholder until
 * the host lookup lands). */
export interface ConversationGit {
	branch: string;
	ahead: number;
	behind: number;
	dirty: number;
}

/** The §E.3 `GetConversationInfo` response (on-demand fold, cached by
 * `(thread_id, cursor)` server-side; the client refreshes on committed-message
 * edges only). */
export interface ConversationInfo {
	threadId: string;
	cursor: number;
	title: string | null;
	cwd: string | null;
	project: string | null;
	/** Canonical `{provider}/{model}` display ref. */
	model: string | null;
	contextWindow: number | null;
	turns: number;
	messages: number;
	models: ConversationModelRow[];
	cumulativeCost: number;
	git: ConversationGit | null;
}

/** Wire form of a background-task snapshot (agent::background_task::TaskSnapshot). */
export interface BackgroundTaskSnapshotWire {
	task_id: string;
	kind: 'MonitorCommand' | 'MonitorWebSocket' | 'BackgroundBash';
	owner_thread_id: string;
	description: string;
	status: 'Running' | 'Stopping' | 'Completed' | 'Failed' | 'TimedOut' | 'Stopped' | 'SessionEnded';
	created_at_ms: number;
	ended_at_ms: number | null;
	event_count: number;
	total_bytes: number;
	exit_code: number | null;
	failure_summary: string | null;
	/** Bounded tail of accumulated output (newest bytes). Omitted by the
	 * sender when empty, so consumers must treat it as optional. */
	output_tail?: string;
}

/** One streamed child-session event from a running sub-agent. */
export type SubagentChildWire =
	| { kind: 'text'; text: string }
	| { kind: 'thinking'; text: string }
	| { kind: 'tool_start'; id: string; name: string; hint?: { key: string; value: string } | null }
	| { kind: 'tool_end'; id: string; name: string; is_error: boolean };
