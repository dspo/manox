// Wire protocol between the TypeScript host and the manox actor. Single
// source of truth; the Rust side mirrors it in
// crates/manox-actor/src/{actor,events}.rs (exposed via manox-napi).
//
// Every event carries `sessionId` except the global ones (`ready`, `models`,
// and errors raised before a session exists). Every command carries
// `sessionId` except `init`, `list_models`, and `shutdown`.
//
// [P2] deferred by design: list_sessions / restore_session,
// set_reasoning_effort, image attachments, subagent_* and plan_* events.

export type Command =
	| { cmd: 'init'; cwd: string }
	| { cmd: 'create_session'; sessionId: string; cwd: string }
	| { cmd: 'dispose_session'; sessionId: string }
	| { cmd: 'submit'; sessionId: string; text: string }
	| { cmd: 'approve'; sessionId: string; id: string; allow: boolean }
	| { cmd: 'cancel_turn'; sessionId: string }
	| { cmd: 'set_model'; sessionId: string; id: string }
	| { cmd: 'get_current_model'; sessionId: string }
	| { cmd: 'list_models' }
	| { cmd: 'get_usage'; sessionId: string }
	| { cmd: 'shutdown' };

export type ToolCallStatus = 'pending' | 'running' | 'completed' | 'failed' | 'cancelled' | string;

export interface ModelInfo {
	id: string;
	name: string;
	provider: string;
}

export interface TokenUsageSnapshot {
	input_tokens?: number;
	output_tokens?: number;
	cache_creation_input_tokens?: number;
	cache_read_input_tokens?: number;
}

export type ActorEvent =
	// lifecycle
	| { type: 'ready' }
	| { type: 'session_created'; sessionId: string }
	| { type: 'session_disposed'; sessionId: string }
	// turn
	| { type: 'turn_started'; sessionId: string }
	| { type: 'turn_finished'; sessionId: string; cancelled: boolean; failed: boolean }
	| { type: 'stop'; sessionId: string; reason: string | null }
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
	| { type: 'approval_mode_changed'; sessionId: string; mode: string }
	| { type: 'current_model'; sessionId: string; id: string | null; name?: string }
	| { type: 'models'; models: ModelInfo[] }
	| { type: 'usage'; sessionId: string; usage: TokenUsageSnapshot }
	| { type: 'error'; sessionId?: string | null; message: string };

/** Events routed per session; everything except the global few. */
export function isSessionEvent(ev: ActorEvent): ev is ActorEvent & { sessionId: string } {
	return typeof (ev as { sessionId?: unknown }).sessionId === 'string';
}
