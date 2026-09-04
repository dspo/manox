// Runtime frame guards for protocol v2 (§D.8, J.5) — hand-written exact-key
// validators in the dsh stream-protocol style (exactKeys + discriminated
// unions). The ts-rs bindings in `protocol.ts` are compile-time truth; these
// guards are the run-time boundary for frames arriving over WS/postMessage.
//
// Rules:
// - Unknown variant tags are NOT errors: callers drop + log (L12 tolerance);
//   every parser returns `{ ok: false, reason }` instead of throwing.
// - Exact-keys checking: a frame with extra keys is rejected — wire drift
//   must surface loudly on the side that introduced it.
// - u64 fields are generated as `bigint` by ts-rs but travel as JSON numbers
//   (seq/cursor stay far below 2^53); the guards accept `number` and cast.

export type GuardOk<T> = { ok: true; value: T };
export type GuardErr = { ok: false; reason: string };
export type Guard<T> = GuardOk<T> | GuardErr;

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v !== null && !Array.isArray(v);

const typeofOr = (v: unknown, t: 'string' | 'boolean' | 'number'): boolean =>
  typeof v === t;

/** dsh-style exact-keys check: `v` must be a record with exactly these keys. */
export function exactKeys(v: unknown, keys: readonly string[]): boolean {
  if (!isRecord(v)) return false;
  const own = Object.keys(v);
  if (own.length !== keys.length) return false;
  return keys.every((k) => k in v);
}

// ── declared surfaces (mirrored; source of truth: src/surface.rs) ──────────

/** §C.2 journal entry tags (JOURNAL_ENTRIES in src/surface.rs). */
export const JOURNAL_ENTRY_TAGS: readonly string[] = [
  'message', 'uiNote', 'turnStart', 'turnFinish', 'stop', 'retry', 'error',
  'agentTextDelta', 'agentThinkingDelta', 'toolCall', 'toolResult',
  'toolOutputChunk', 'subagentChild', 'subagentProgress', 'modelChange',
  'cwdChange', 'projectChange', 'permissionModeChange',
  'reasoningEffortChange', 'planModeChange', 'planUpdate', 'goal', 'title',
  'browserSuites', 'backgroundTask', 'approval', 'pinnedArchived',
  'compaction', 'compactionStarted', 'branchSummary', 'label', 'sessionInfo',
  'leaf', 'metrics',
];

/** §D.5 host event tags (HOST_EVENTS in src/surface.rs). */
export const HOST_EVENT_TAGS: readonly string[] = [
  'ready', 'models', 'commands', 'threadsUpdated', 'sessionStatus',
  'sessionCreated', 'sessionDisposed', 'error',
];

export const isKnownJournalTag = (tag: unknown): tag is string =>
  typeof tag === 'string' && JOURNAL_ENTRY_TAGS.includes(tag);

export const isKnownHostTag = (tag: unknown): tag is string =>
  typeof tag === 'string' && HOST_EVENT_TAGS.includes(tag);

// ── StreamFrame (§D.1) ──────────────────────────────────────────────────────

export type FrameSnapshot = {
  type: 'snapshot';
  sessionId: string;
  header: unknown;
  cursor: number;
  records: unknown[];
  hasMore: boolean;
  projections: Record<string, unknown>;
  projectionsAsOfSeq: number;
};
export type FrameEntry = { type: 'entry'; seq: number; event: Record<string, unknown> };
export type FrameProjections = {
  type: 'projections';
  sessionId: string;
  asOfSeq: number;
  values: Record<string, unknown>;
};
export type StreamFrameShape = FrameSnapshot | FrameEntry | FrameProjections;

export function parseStreamFrame(v: unknown): Guard<StreamFrameShape> {
  if (!isRecord(v) || typeofOr(v.type, 'string') === false) {
    return { ok: false, reason: 'stream frame: missing string "type"' };
  }
  switch (v.type) {
    case 'snapshot':
      if (
        !exactKeys(v, [
          'type', 'sessionId', 'header', 'cursor', 'records', 'hasMore',
          'projections', 'projectionsAsOfSeq',
        ])
      ) return { ok: false, reason: 'snapshot: exact-keys check failed' };
      if (
        !typeofOr(v.sessionId, 'string') || !typeofOr(v.cursor, 'number')
        || !Array.isArray(v.records) || !typeofOr(v.hasMore, 'boolean')
        || !isRecord(v.projections) || !typeofOr(v.projectionsAsOfSeq, 'number')
      ) return { ok: false, reason: 'snapshot: field types' };
      return {
        ok: true,
        value: v as unknown as FrameSnapshot,
      };
    case 'entry':
      if (!exactKeys(v, ['type', 'seq', 'event'])) {
        return { ok: false, reason: 'entry: exact-keys check failed' };
      }
      if (
        !typeofOr(v.seq, 'number') || !isRecord(v.event)
      ) return { ok: false, reason: 'entry: field types' };
      return { ok: true, value: v as unknown as FrameEntry };
    case 'projections':
      if (!exactKeys(v, ['type', 'sessionId', 'asOfSeq', 'values'])) {
        return { ok: false, reason: 'projections: exact-keys check failed' };
      }
      if (
        !typeofOr(v.sessionId, 'string') || !typeofOr(v.asOfSeq, 'number')
        || !isRecord(v.values)
      ) return { ok: false, reason: 'projections: field types' };
      return { ok: true, value: v as unknown as FrameProjections };
    default:
      // L12 tolerance: unknown frame tags drop + log at the call site.
      return { ok: false, reason: `stream frame: unknown type ${(v.type as string)}` };
  }
}

// ── StreamEndReason (§D.1) ──────────────────────────────────────────────────

export type StreamEndReasonShape =
  | { type: 'closed' } | { type: 'cancelled' } | { type: 'resync' }
  | { type: 'failure'; code: string; message: string };

export function parseStreamEndReason(v: unknown): Guard<StreamEndReasonShape> {
  if (!isRecord(v) || !typeofOr(v.type, 'string')) {
    return { ok: false, reason: 'stream end: missing string "type"' };
  }
  switch (v.type) {
    case 'closed':
    case 'cancelled':
    case 'resync':
      if (!exactKeys(v, ['type'])) {
        return { ok: false, reason: `${v.type}: exact-keys check failed` };
      }
      return { ok: true, value: v as StreamEndReasonShape };
    case 'failure':
      if (!exactKeys(v, ['type', 'code', 'message'])) {
        return { ok: false, reason: 'failure: exact-keys check failed' };
      }
      if (!typeofOr(v.code, 'string') || !typeofOr(v.message, 'string')) {
        return { ok: false, reason: 'failure: field types' };
      }
      return { ok: true, value: v as StreamEndReasonShape };
    default:
      return { ok: false, reason: `stream end: unknown type ${(v.type as string)}` };
  }
}

// ── HostEvent (§D.5) — shallow shape, per-arm exact keys ────────────────────

export type HostEventShape = { type: string } & Record<string, unknown>;

export function parseHostEvent(v: unknown): Guard<HostEventShape> {
  if (!isRecord(v) || !isKnownHostTag(v.type)) {
    return { ok: false, reason: 'host event: unknown or missing tag' };
  }
  const arms: Record<string, readonly string[]> = {
    ready: ['type', 'epoch'],
    models: ['type', 'models'],
    commands: ['type', 'commands'],
    threadsUpdated: ['type', 'threads'],
    sessionStatus: [
      'type', 'sessionId', 'running', 'errored', 'unread', 'pendingAuth',
      'pendingPlan', 'backgroundWork',
    ],
    sessionCreated: ['type', 'sessionId', 'header'],
    sessionDisposed: ['type', 'sessionId'],
    error: ['type', 'message'],
  };
  const keys = arms[v.type];
  if (!exactKeys(v, keys)) {
    return { ok: false, reason: `host event ${v.type}: exact-keys check failed` };
  }
  if (v.type !== 'ready' && !typeofOr(v.sessionId, 'string') && v.type !== 'models'
    && v.type !== 'commands' && v.type !== 'threadsUpdated' && v.type !== 'error') {
    return { ok: false, reason: `host event ${v.type}: sessionId must be a string` };
  }
  return { ok: true, value: v as HostEventShape };
}

// ── JournalWireEvent (§C.2) — tag-level tolerant validation ────────────────
//
// Per-arm field validation lives with the consumers that need specific
// payloads; the boundary contract here is: record + known tag + payload keys
// are a subset of the declared vocabulary (the wire never carries extra
// top-level keys beyond the variant fields — flatten rule §C.1).

export type JournalEventShape = { type: string } & Record<string, unknown>;

export function parseJournalEvent(v: unknown): Guard<JournalEventShape> {
  if (!isRecord(v)) return { ok: false, reason: 'journal event: not a record' };
  if (!isKnownJournalTag(v.type)) {
    return { ok: false, reason: `journal event: unknown tag ${String(v.type)}` };
  }
  return { ok: true, value: v as JournalEventShape };
}
