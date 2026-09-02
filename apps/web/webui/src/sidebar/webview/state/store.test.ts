// Store behaviour: per-thread ServerNote folding, tool-card folding, restored
// history mapping, and the view/thread bookkeeping around them. The store is
// driven by `FromServer` push delivery — `Response` frames are dropped by the
// bridge before dispatch, so only `Notification` / `Request` (ServerCall)
// frames reach the fold.

import { describe, expect, it, vi } from 'vitest';

import type {
	FromServer,
	ServerCall,
	ServerNote,
	ThreadListItem,
	WireMessage,
} from '../../../protocol';
import { Store, wireMessagesToTranscriptItems } from './store';
import { foldToolStatus, userTurnHeader } from './transcript';

/** Wrap a ServerNote as a `FromServer` notification. */
const note = (n: ServerNote): FromServer => ({ kind: 'notification', note: n });

/** Wrap a ServerCall as a `FromServer` request. */
const callReq = (id: string, c: ServerCall): FromServer => ({ kind: 'request', id, call: c });

const listItem = (partial: Partial<ThreadListItem> & { id: string }): ThreadListItem => ({
	title: 't',
	updated_at: 1,
	running: false,
	unread: false,
	errored: false,
	pending_auth: false,
	pending_plan: false,
	background_work: false,
	model_id: 'm',
	pinned: false,
	archived: false,
	parent_id: null,
	depth: 0,
	...partial,
});

/** A fresh session: the store switches view on `SessionCreated`. */
const startSession = (sessionId = 's'): Store => {
	const store = new Store();
	store.dispatch(note({ method: 'sessionCreated', sessionId }));
	return store;
};

/** A restored session: open then replay history with `restored: true`. */
const restoreSession = (sessionId = 's', messages: WireMessage[] = []): Store => {
	const store = new Store();
	store.openRemote(sessionId);
	store.dispatch(
		note({
			method: 'sessionCreated',
			sessionId,
		}),
	);
	store.dispatch(
		note({
			method: 'threadHistory',
			sessionId,
			messages,
			displayHistory: [],
			autoApprovedTools: null,
			restored: true,
			loading: false,
		}),
	);
	return store;
};

const thread = (store: Store, id = 's') => store.get().perThread[id];

const toolCard = (store: Store, id: string, sessionId = 's') => {
	const item = thread(store, sessionId)?.items.find((i) => i.kind === 'tool' && i.id === id);
	return item && item.kind === 'tool' ? item.tool : undefined;
};

describe('foldToolStatus', () => {
	it('folds terminal wire statuses into UI semantics', () => {
		expect(foldToolStatus('success')).toBe('completed');
		expect(foldToolStatus('error')).toBe('failed');
		expect(foldToolStatus('denied')).toBe('denied');
		expect(foldToolStatus('cancelled')).toBe('cancelled');
	});
	it('passes authorization and progress statuses through', () => {
		expect(foldToolStatus('pending-approval')).toBe('pending-approval');
		expect(foldToolStatus('running')).toBe('running');
	});
	it('treats unknown statuses as running', () => {
		expect(foldToolStatus('weird')).toBe('running');
	});
});

describe('thread routing', () => {
	it('SessionCreated opens the conversation view with a fresh thread state', () => {
		const store = startSession('s');
		expect(store.get().view).toBe('conversation');
		expect(store.get().activeThreadId).toBe('s');
		expect(thread(store, 's')).toBeTruthy();
	});
	it('a restored session starts in the loading state via openRemote', () => {
		const store = new Store();
		store.openRemote('s');
		expect(thread(store, 's').loading).toBe(true);
		store.dispatch(
			note({
				method: 'threadHistory',
				sessionId: 's',
				messages: [],
				displayHistory: [],
				autoApprovedTools: null,
				restored: true,
				loading: false,
			}),
		);
		expect(thread(store, 's').loading).toBe(false);
	});
	it('routes events to their own thread only', () => {
		const store = startSession('a');
		store.dispatch(note({ method: 'sessionCreated', sessionId: 'b' }));
		store.dispatch(note({ method: 'agentText', sessionId: 'a', text: 'hi' }));
		expect(thread(store, 'a').items).toHaveLength(1);
		expect(thread(store, 'b').items).toHaveLength(0);
	});
	it('sessionDisposed drops the thread and falls back to the list', () => {
		const store = startSession('s');
		store.dispatch(note({ method: 'sessionDisposed', sessionId: 's' }));
		expect(store.get().perThread['s']).toBeUndefined();
		expect(store.get().view).toBe('threads');
	});
	it('openLocal and backToList switch views without touching thread state', () => {
		const store = startSession('s');
		store.backToList();
		expect(store.get().view).toBe('threads');
		store.openLocal('s');
		expect(store.get().view).toBe('conversation');
	});
});

describe('home-composer drafts', () => {
	it('draftThread opens the conversation view with the echoed first message', () => {
		const store = new Store();
		store.draftThread('d1', 'hello');
		expect(store.get().view).toBe('conversation');
		expect(store.isCreating('d1')).toBe(true);
		expect(thread(store, 'd1').items[0]).toMatchObject({ kind: 'user', text: 'hello' });
	});
	it('SessionCreated clears the draft guard and keeps the echoed items', () => {
		const store = new Store();
		store.draftThread('d1', 'hello');
		store.dispatch(note({ method: 'sessionCreated', sessionId: 'd1' }));
		expect(store.isCreating('d1')).toBe(false);
		expect(thread(store, 'd1').items).toHaveLength(1);
	});
	it('a global error during a pending draft releases the guard and returns to the list', () => {
		const store = new Store();
		store.draftThread('d1', 'hello');
		store.dispatch(note({ method: 'error', sessionId: null, message: 'boom' }));
		expect(store.isCreating('d1')).toBe(false);
		expect(store.get().view).toBe('threads');
		expect(store.get().perThread['d1']).toBeUndefined();
	});
	it('a global error without a pending draft leaves the view alone', () => {
		const store = startSession('s');
		store.dispatch(note({ method: 'error', sessionId: null, message: 'boom' }));
		expect(store.get().view).toBe('conversation');
	});
});

describe('transcript folding', () => {
	it('streams text into the trailing item of the same kind', () => {
		const store = startSession();
		store.dispatch(note({ method: 'agentText', sessionId: 's', text: 'hel' }));
		store.dispatch(note({ method: 'agentText', sessionId: 's', text: 'lo' }));
		expect(thread(store).items).toHaveLength(1);
		expect(thread(store).items[0]).toMatchObject({ kind: 'assistant', text: 'hello' });
	});
	it('stamps assistant items with the model the turn started with', () => {
		const store = startSession();
		store.dispatch(note({ method: 'currentModel', sessionId: 's', id: 'gpt-5', name: null }));
		store.dispatch(note({ method: 'turnStarted', sessionId: 's' }));
		store.dispatch(note({ method: 'agentText', sessionId: 's', text: 'hi' }));
		expect(thread(store).items[0]).toMatchObject({ modelId: 'gpt-5' });
	});
	it('inserts tool cards with folded wire status', () => {
		const store = startSession();
		store.dispatch(
			note({
				method: 'toolCall',
				sessionId: 's',
				id: 't1',
				name: 'Bash',
				title: 'Bash',
				status: 'running',
				input: null,
			}),
		);
		expect(toolCard(store, 't1')?.status).toBe('running');
		store.dispatch(
			note({
				method: 'toolCall',
				sessionId: 's',
				id: 't1',
				name: 'Bash',
				title: 'Bash',
				status: 'success',
				input: null,
			}),
		);
		expect(toolCard(store, 't1')?.status).toBe('completed');
	});
	it('records live output and the final result', () => {
		const store = startSession();
		store.dispatch(
			note({ method: 'toolOutput', sessionId: 's', id: 't1', chunk: 'out-' }),
		);
		store.dispatch(note({ method: 'toolOutput', sessionId: 's', id: 't1', chunk: 'put' }));
		store.dispatch(
			note({ method: 'toolResult', sessionId: 's', id: 't1', output: 'output', isError: false }),
		);
		expect(toolCard(store, 't1')?.output).toBe('output');
		expect(toolCard(store, 't1')?.status).toBe('completed');
	});
	it('approvalDecision allow stamps the tool card badge', () => {
		const store = startSession();
		store.dispatch(
			note({
				method: 'toolCall',
				sessionId: 's',
				id: 't1',
				name: 'Bash',
				title: 'Bash',
				status: 'running',
				input: null,
			}),
		);
		store.dispatch(
			note({
				method: 'approvalDecision',
				sessionId: 's',
				toolCallId: 't1',
				toolName: 'Bash',
				toolTitle: 'Bash',
				verdict: 'allow',
				reason: null,
			}),
		);
		expect(toolCard(store, 't1')?.autoApproved).toBe(true);
	});
	it('drives the turn flag', () => {
		const store = startSession();
		store.dispatch(note({ method: 'turnStarted', sessionId: 's' }));
		expect(thread(store).turnActive).toBe(true);
		store.dispatch(
			note({
				method: 'turnFinished',
				sessionId: 's',
				cancelled: false,
				failed: false,
				strandedSteerIds: [],
			}),
		);
		expect(thread(store).turnActive).toBe(false);
	});
	it('routes session errors to their own thread only', () => {
		const store = startSession('a');
		store.dispatch(note({ method: 'sessionCreated', sessionId: 'b' }));
		store.dispatch(note({ method: 'error', sessionId: 'a', message: 'boom' }));
		expect(thread(store, 'a').error).toBe('boom');
		expect(thread(store, 'b').error).toBeNull();
	});
	it('clears a thread error when its next turn starts', () => {
		const store = startSession();
		store.dispatch(note({ method: 'error', sessionId: 's', message: 'boom' }));
		store.dispatch(note({ method: 'turnStarted', sessionId: 's' }));
		expect(thread(store).error).toBeNull();
	});
	it('tracks model, approval-mode, and reasoning-effort changes per thread', () => {
		const store = startSession();
		store.dispatch(note({ method: 'currentModel', sessionId: 's', id: 'm1', name: null }));
		store.dispatch(note({ method: 'permissionModeChanged', sessionId: 's', mode: 'read-only' }));
		store.dispatch(note({ method: 'reasoningEffortChanged', sessionId: 's', effort: 'max' }));
		expect(thread(store).currentModelId).toBe('m1');
		expect(thread(store).approvalMode).toBe('read-only');
		expect(thread(store).reasoningEffort).toBe('max');
	});
});

describe('approval ServerCall cards', () => {
	it('adds an approval card from a ServerCall and keeps it on decision', () => {
		const store = startSession();
		store.dispatch(
			callReq('auth1', {
				method: 'approve',
				sessionId: 's',
				authId: 'auth1',
				toolName: 'Bash',
				summary: 'rm -rf',
				input: null,
			}),
		);
		const card = thread(store).items.find((i) => i.kind === 'approval');
		expect(card).toMatchObject({ kind: 'approval', id: 'auth1', toolName: 'Bash' });
		store.decideApproval('s', 'auth1');
		expect(thread(store).items.find((i) => i.kind === 'approval')).toBeUndefined();
	});
});

describe('plan, goal, and compaction events', () => {
	it('folds plan mode and goal changes into the info card', () => {
		const store = startSession();
		store.dispatch(note({ method: 'planModeChanged', sessionId: 's', enabled: true }));
		expect(thread(store).planMode).toBe(true);
		store.dispatch(
			note({
				method: 'goalChanged',
				sessionId: 's',
				snapshot: { thread_id: 's', goal_id: 'g', objective: 'o', status: 'active' },
			}),
		);
		expect(thread(store).info?.goal).toMatchObject({ objective: 'o' });
	});
	it('renders a live compaction as a transcript recap item', () => {
		const store = startSession();
		store.dispatch(note({ method: 'compaction', sessionId: 's', summary: 'compacted', retained: [] }));
		expect(thread(store).items.find((i) => i.kind === 'compaction')).toMatchObject({
			summary: 'compacted',
		});
	});
	it('PlanVerdict ServerCall stages a review card', () => {
		const store = startSession();
		store.dispatch(
			callReq('s', {
				method: 'planVerdict',
				sessionId: 's',
				planFile: '/p.md',
				title: 'T',
				content: 'body',
			}),
		);
		expect(thread(store).pendingPlan).toMatchObject({ planFile: '/p.md', content: 'body' });
		store.clearPlanReview('s');
		expect(thread(store).pendingPlan).toBeNull();
	});
});

describe('global folds', () => {
	it('stores models, commands, threads, and surfaces global errors', () => {
		const store = startSession();
		store.dispatch(note({ method: 'models', models: [] }));
		store.dispatch(note({ method: 'commands', commands: [] }));
		store.dispatch(note({ method: 'error', sessionId: null, message: 'boom' }));
		expect(store.get().models).toEqual([]);
		expect(store.get().commands).toEqual([]);
		expect(store.get().error).toBe('boom');
	});
	it('threads snapshots sync titles and running into live thread states', () => {
		const store = startSession('a');
		store.dispatch(note({ method: 'sessionCreated', sessionId: 'b' }));
		store.dispatch(
			note({
				method: 'threadsUpdated',
				threads: [listItem({ id: 'a', title: 'A', running: true }), listItem({ id: 'b' })],
			}),
		);
		expect(thread(store, 'a').title).toBe('A');
		expect(thread(store, 'a').turnActive).toBe(true);
	});
});

describe('turn-active recovery', () => {
	it('syncs turnActive from the thread list running flag', () => {
		const store = startSession('a');
		store.dispatch(
			note({
				method: 'threadsUpdated',
				threads: [listItem({ id: 'a', running: true })],
			}),
		);
		expect(thread(store, 'a').turnActive).toBe(true);
	});
});

describe('steer lifecycle', () => {
	it('marks a queued bubble pending then clears it on injection', () => {
		const store = startSession();
		store.echoUser('s', 'go', undefined, { queued: true, clientId: 'c1' });
		store.dispatch(
			note({ method: 'steerPending', sessionId: 's', clientId: 'c1', messageId: 'mid' }),
		);
		expect(thread(store).items[0]).toMatchObject({ steerPendingId: 'mid' });
		store.dispatch(note({ method: 'steerInjected', sessionId: 's', messageId: 'mid' }));
		expect((thread(store).items[0] as { steerPendingId?: string | null }).steerPendingId).toBeNull();
	});
});

describe('wireMessagesToTranscriptItems', () => {
	const userMsg = (partial: Partial<WireMessage> = {}): WireMessage => ({
		id: 'u1',
		timestamp: 1,
		parent_id: null,
		provenance: 'user',
		role: 'user',
		content: [{ Text: 'hello' }],
		...partial,
	});
	it('maps user text with ui metadata and deflated images', () => {
		const items = wireMessagesToTranscriptItems([
			userMsg({
				ui: { display_text: '/greet', model_id: 'm' },
				content: [{ Text: 'expanded body' }, { Image: { mime_type: 'image/png', byte_len: 99 } }],
			}),
		]);
		expect(items[0]).toMatchObject({ kind: 'user', text: 'expanded body', displayText: '/greet' });
		expect((items[0] as { images?: { byteLen: number }[] }).images?.[0]).toMatchObject({ mimeType: 'image/png', byteLen: 99 });
	});
	it('passes the authoring agent through as the turn header from', () => {
		expect(userTurnHeader({ agent: 'researcher' }, '', null, null)).toBe('researcher');
	});
	it('skips empty user messages', () => {
		const items = wireMessagesToTranscriptItems([userMsg({ content: [] })]);
		expect(items).toHaveLength(0);
	});
	it('maps assistant text and tool-use blocks into transcript items', () => {
		const items = wireMessagesToTranscriptItems([
			{
				id: 'a1',
				timestamp: 2,
				parent_id: null,
				provenance: 'assistant',
				role: 'assistant',
				content: [
					{ Text: 'sure' },
					{ ToolUse: { id: 't1', name: 'Bash', raw_input: 'ls', input: {}, is_input_complete: true, thought_signature: null } },
				],
			},
		]);
		expect(items).toHaveLength(2);
		expect(items[1]).toMatchObject({ kind: 'tool', id: 't1' });
	});
});

describe('threadInfo payload merge', () => {
	it('maps ThreadInfoPayload fields into thread state', () => {
		const store = startSession();
		store.dispatch(
			note({
				method: 'threadInfo',
				sessionId: 's',
				info: {
					cwd: '/w',
					project: null,
					displayTitle: 'My Thread',
					modelId: 'm1',
					modelName: 'M1',
					model: null,
					permissionMode: 'read-only',
					reasoningEffort: 'max',
					pinned: true,
					archived: false,
					depth: 0,
					agentLabel: 'lead',
					selfAuthor: 'lead',
					cwdPath: '/w',
					branch: 'main',
					goal: null,
					goalElapsedSeconds: null,
					planMode: true,
					browserSuites: [],
					historyPhase: 'idle',
					running: false,
					hasInteracted: true,
				},
			}),
		);
		expect(thread(store).title).toBe('My Thread');
		expect(thread(store).currentModelId).toBe('m1');
		expect(thread(store).approvalMode).toBe('read-only');
		expect(thread(store).planMode).toBe(true);
		expect(thread(store).branch).toBe('main');
		expect(thread(store).info?.cwd_path).toBe('/w');
	});
});

describe('subagent and background tasks', () => {
	it('aggregates sub-agent start and progress into the info snapshot', () => {
		const store = startSession();
		store.dispatch(
			note({
				method: 'subagentStarted',
				sessionId: 's',
				id: 'sa1',
				agentType: 'researcher',
				description: 'digging',
			}),
		);
		expect(thread(store).info?.agents[0]).toMatchObject({ id: 'sa1', agent_type: 'researcher' });
		store.dispatch(
			note({
				method: 'subagentProgress',
				sessionId: 's',
				id: 'sa1',
				agentType: 'researcher',
				toolUses: 3,
				latestActivity: 'x',
				status: 'running',
			}),
		);
		expect(thread(store).info?.agents[0]?.tool_uses).toBe(3);
	});
	it('adds a background-task card and keeps the snapshot map live', () => {
		const store = startSession();
		store.dispatch(
			note({
				method: 'backgroundTaskUpdated',
				sessionId: 's',
				snapshot: {
					task_id: 'bg1',
					kind: 'BackgroundBash',
					owner_thread_id: 's',
					description: 'watch',
					status: 'Running',
					created_at_ms: 1,
					ended_at_ms: null,
					event_count: 0,
					total_bytes: 0,
					exit_code: null,
					failure_summary: null,
				},
			}),
		);
		expect(thread(store).items.find((i) => i.kind === 'background_task')).toBeTruthy();
		expect(thread(store).backgroundTasks['bg1']).toBeTruthy();
	});
});
