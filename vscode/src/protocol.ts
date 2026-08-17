// Wire protocol between the TypeScript host and the manox actor. Single
// source of truth; the Rust side mirrors it in
// crates/manox-actor/src/{actor,events}.rs (exposed via manox-napi).
//
// Every event carries `sessionId` except the global ones (`ready`, `models`,
// `threads_updated`, `commands`, `model_*`, and errors raised before a
// session exists).
// Every command carries `sessionId` except `init`, `list_models`,
// `list_threads`, `list_commands`, `model_chat`, and `cancel_model_chat`.
// Actor shutdown does not go over this
// protocol — the napi binding terminates the thread directly.

export type Command =
	| { cmd: 'init'; cwd: string; host: 'vscode' }
	| { cmd: 'create_session'; sessionId: string; cwd: string }
	| { cmd: 'dispose_session'; sessionId: string }
	| {
			cmd: 'submit';
			sessionId: string;
			text: string;
			images?: ImageAttachment[];
			/** Echo identifier the host relays back on `steer_pending` /
			 * `drop_queued`; absent on submits from older surfaces. */
			clientId?: string;
	  }
	| {
			cmd: 'steer';
			sessionId: string;
			clientId: string;
			text: string;
			images?: ImageAttachment[];
	  }
	| { cmd: 'drop_queued'; sessionId: string; clientId: string }
	| { cmd: 'approve'; sessionId: string; id: string; allow: boolean }
	| { cmd: 'set_approval_mode'; sessionId: string; mode: ApprovalMode }
	| { cmd: 'cancel_turn'; sessionId: string }
	| { cmd: 'set_model'; sessionId: string; id: string }
	| { cmd: 'set_reasoning_effort'; sessionId: string; effort: ReasoningEffort }
	| { cmd: 'get_current_model'; sessionId: string }
	| { cmd: 'list_models' }
	| { cmd: 'get_usage'; sessionId: string }
	| { cmd: 'list_threads' }
	| { cmd: 'archive_thread'; sessionId: string; archived: boolean }
	| { cmd: 'pin_thread'; sessionId: string; pinned: boolean }
	| { cmd: 'open_thread'; sessionId: string }
	| { cmd: 'focus_thread'; sessionId?: string }
	| { cmd: 'list_commands' }
	| { cmd: 'thread_info'; sessionId: string }
	| { cmd: 'model_chat'; requestId: string; model: string; messages: ModelChatMessage[]; tools: ModelChatTool[] }
	| { cmd: 'cancel_model_chat'; requestId: string }
	| { cmd: 'set_plan_mode'; sessionId: string; enabled: boolean }
	| { cmd: 'plan_verdict'; sessionId: string; choice: PlanVerdictChoice }
	| { cmd: 'plan_seed_execution'; sessionId: string; planFile: string }
	| { cmd: 'goal'; sessionId: string; action: GoalAction; objective?: string; budget?: number }
	| { cmd: 'stop_background_task'; sessionId: string; taskId: string }
	| {
			cmd: 'answer_question';
			sessionId: string;
			id: string;
			/** (question text, selected labels joined by ", "). */
			answers: [string, string][];
			/** Free-form supplemental note; rides alongside the selections. */
			response: string | null;
	  };

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

/** Base64-encoded image attachment (submit payload wire form). */
export interface ImageAttachment {
	data: string;
	mimeType: string;
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

/** Tool-authorization policy: autopilot gates on the safety reviewer with
 * user escalation, danger runs every tool call without prompting. */
export type ApprovalMode = 'autopilot' | 'danger';

/** User-facing reasoning-effort knob for the model dropdown (`high`/`max`). */
export type ReasoningEffort = 'high' | 'max';

export interface ModelInfo {
	id: string;
	name: string;
	provider: string;
	/** Provider display name (e.g. "DeepSeek" for the "DeepSeek-anthropic"
	 * registration id). Absent only from older actors. */
	provider_name?: string;
	/** Wire API shape ("anthropic", "openai_responses", …); drives the
	 * cascade menu's badge and tint. */
	api: string;
	context_window: number;
	/** Per-model output budget; absent from older actors. */
	max_tokens?: number;
}

export interface TokenUsageSnapshot {
	input_tokens?: number;
	output_tokens?: number;
	cache_creation_input_tokens?: number;
	cache_read_input_tokens?: number;
}

/** One row in the threads list (snake_case wire form from the actor). */
export interface ThreadListItem {
	id: string;
	title: string;
	/** Unix seconds of the last interaction. */
	updated_at: number;
	running: boolean;
	unread: boolean;
	errored: boolean;
	pending_auth: boolean;
	/** A plan review verdict is due; the row shows the static blue wheel. */
	pending_plan: boolean;
	/** Live monitors / background bash keep the loop self-advancing; the row
	 * spins even with no turn in flight. */
	background_work: boolean;
	model_id: string;
	pinned: boolean;
	archived: boolean;
	/** Leader session id for a team worker row; null for a top-level row. */
	parent_id: string | null;
	/** Nesting level: 0 is top-level, 1 is a team member of a top-level
	 * leader, and so on. */
	depth: number;
}

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

/** One sub-agent's aggregated progress inside a thread info snapshot. */
export interface SubagentSnapshot {
	id: string;
	agent_type: string;
	description: string;
	tool_uses: number;
	latest_activity: string | null;
	status: ToolCallStatus;
}

/** Working-tree change counts for the info card's branch row. */
export interface GitStats {
	added: number;
	deleted: number;
	untracked: number;
}

/** Conversation info panel snapshot (thread_info event payload). */
export interface ThreadInfoSnapshot {
	reasoning_effort: ReasoningEffort;
	worktree_path: string | null;
	plan: PlanSnapshotWire | null;
	goal: GoalSnapshotWire | null;
	usage: TokenUsageSnapshot;
	/** Token usage keyed by "{provider}/{model_id}". */
	per_model_usage?: Record<string, TokenUsageSnapshot>;
	/** Per-model spend keyed like `per_model_usage`. */
	per_model_cost?: Record<string, number>;
	cost: number;
	pending_auth_count: number;
	agents: SubagentSnapshot[];
	/** Arrives via the separate async git_stats event. */
	git_stats?: GitStats;
}
/** Wire form of one restored-history message (serde shape of
 * agent::message::Message). Content blocks are externally tagged; image
 * blocks arrive deflated to `{mime_type, byte_len}` placeholders. */
export interface WireMessage {
	id: string;
	/** Unix seconds. */
	timestamp: number;
	parent_id: string | null;
	provenance:
		| 'user'
		| 'assistant'
		| 'tool'
		| 'goal_continuation'
		| 'goal_objective_update';
	role: 'user' | 'assistant' | 'system';
	content: WireContentBlock[];
	ui?: {
		model_id?: string;
		approval_mode?: number;
		steered?: boolean;
		external_event?: boolean;
		display_text?: string;
	};
}

export type WireContentBlock =
	| { Text: string }
	| { Thinking: { text: string; signature: string | null } }
	| { Image: { mime_type: string; byte_len: number } }
	| {
			ToolUse: {
				id: string;
				name: string;
				raw_input: string;
				input: unknown;
				is_input_complete: boolean;
				thought_signature: string | null;
			};
	  }
	| { ToolResult: { tool_use_id: string; tool_name: string; is_error: boolean; content: string } }
	| { Compaction: string };

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

export type ActorEvent =
	// lifecycle
	| { type: 'ready' }
	| { type: 'session_created'; sessionId: string }
	| { type: 'session_disposed'; sessionId: string }
	// turn
	| { type: 'turn_started'; sessionId: string }
	| {
			type: 'turn_finished';
			sessionId: string;
			cancelled: boolean;
			failed: boolean;
			/** Steer message ids that never made it into the settled run. */
			strandedSteerIds?: string[];
	  }
	| { type: 'stop'; sessionId: string; reason: string | null }
	// queued-message steering
	| {
			type: 'steer_pending';
			sessionId: string;
			/** The client echo id of the original submission. */
			clientId: string;
			/** Kernel message id of the injected steer. */
			messageId: string;
	  }
	| { type: 'steer_injected'; sessionId: string; messageId: string }
	// content
	| { type: 'agent_text'; sessionId: string; text: string }
	| { type: 'agent_thinking'; sessionId: string; text: string }
	// tools
	| {
			type: 'tool_call';
			sessionId: string;
			id: string;
			name: string;
			title: string;
			status: ToolCallStatus;
			input?: unknown;
	  }
	| { type: 'tool_output'; sessionId: string; id: string; chunk: string }
	| { type: 'tool_result'; sessionId: string; id: string; output: string; is_error: boolean }
	| {
			type: 'tool_call_authorization';
			sessionId: string;
			id: string;
			tool_name: string;
			summary: string;
			input: unknown;
	  }
	// state
	| { type: 'model_changed'; sessionId: string; from: string | null; to: string }
	| { type: 'reasoning_effort_changed'; sessionId: string; effort: ReasoningEffort }
	| { type: 'approval_mode_changed'; sessionId: string; mode: ApprovalMode }
	| { type: 'current_model'; sessionId: string; id: string | null; name?: string }
	| { type: 'models'; models: ModelInfo[] }
	| { type: 'usage'; sessionId: string; usage: TokenUsageSnapshot; cost: number }
	// bare-model completion (stateless; keyed by requestId, no sessionId)
	| { type: 'model_text'; requestId: string; text: string }
	| { type: 'model_thinking'; requestId: string; text: string }
	| { type: 'model_tool_call'; requestId: string; id: string; name: string; input: unknown }
	| { type: 'model_chat_done'; requestId: string; stop: string | null; error: string | null }
	| {
			type: 'token_usage';
			sessionId: string;
			input: number;
			output: number;
			cache_creation: number;
			cache_read: number;
	  }
	// threads / registry
	| { type: 'threads_updated'; threads: ThreadListItem[] }
	| { type: 'commands'; commands: CommandEntry[] }
	// restored-history and info snapshots
	| { type: 'thread_history'; sessionId: string; messages: WireMessage[] }
	| { type: 'thread_info'; sessionId: string; info: ThreadInfoSnapshot }
	| { type: 'branch'; sessionId: string; branch: string }
	| { type: 'git_stats'; sessionId: string; stats: GitStats }
	| { type: 'history_progress'; sessionId: string }
	// plan / goal / worktree / sub-agents
	| { type: 'plan_ready'; sessionId: string; plan_file: string; title: string; content?: string }
	| { type: 'plan_updated'; sessionId: string; snapshot: PlanSnapshotWire | null }
	| { type: 'plan_mode_changed'; sessionId: string; enabled: boolean }
	| { type: 'goal_changed'; sessionId: string; snapshot: GoalSnapshotWire | null }
	| { type: 'worktree_changed'; sessionId: string; active: boolean; path: string | null }
	| { type: 'compaction'; sessionId: string; summary: string }
	| { type: 'background_task_updated'; sessionId: string; snapshot: BackgroundTaskSnapshotWire }
	| { type: 'subagent_child'; sessionId: string; id: string; event: SubagentChildWire }
	| {
			type: 'subagent_started';
			sessionId: string;
			id: string;
			agent_type: string;
			description: string;
	  }
	| {
			type: 'subagent_progress';
			sessionId: string;
			id: string;
			agent_type: string;
			tool_uses: number;
			latest_activity: string | null;
			status: ToolCallStatus;
	  }
	| { type: 'error'; sessionId?: string | null; message: string };

/** Events routed per session; everything except the global few. */
export function isSessionEvent(ev: ActorEvent): ev is ActorEvent & { sessionId: string } {
	return typeof (ev as { sessionId?: unknown }).sessionId === 'string';
}
