// Slot registry — the L5 extension surface's typed composition core
// (§G; dsh `ui-slots` pure core translated to this repo). Owners declare slot
// contracts by merging into {@link SlotMap} (module augmentation, see
// `slots.registry.ts`); one {@link register} call contributes a component
// into a declared slot. Zero framework dependencies — the React binding
// lives in `slots.outlet.tsx` / `state/hooks.ts`.
//
// Discipline (§G): panels only ever `register`/`inject`; a component never
// imports another panel's component. Private data reaches a component
// through the registration closure, shared data through store selectors.
//
// Semantics (ported from dsh ui-slots/src/index.ts `SlotCore`):
// - `single`: one cell; the lowest-priority live entry renders; a second
//   registration at an occupied priority throws (shadow it with a different
//   priority instead).
// - `list`: cells addressed by required `id`, ordered by `priority` then
//   explicit `order` then registration sequence; shadowing per `id`.
// - `keyed`: cells addressed by required `key`; the outlet renders the
//   winner for the dispatched key.
// - `inject(slot, cb)`: run `cb` for each declaration lifetime of the slot —
//   now if already declared, on each fresh declaration; the callback's
//   returned disposer is called when the declaration collapses. This is how
//   a registrant contributes into a slot whose owner may mount/unmount
//   repeatedly without losing its contribution.
// - Change propagation: per-key versions bump synchronously; entry listeners
//   notify batched per microtask (N same-tick mutations → one notification
//   per touched key); declaration listeners fire synchronously so inject
//   lifetimes settle before a same-tick re-registration can observe stale
//   state.

import type { ReactNode } from 'react';

/**
 * Slot contract table. Owners extend it via `declare module` (the webui's
 * first-batch declarations converge in `slots.registry.ts`); every entry
 * satisfies {@link SlotEntryDef}.
 */
// eslint-disable-next-line @typescript-eslint/no-empty-object-type
export interface SlotMap {}

/** Slot cardinality: single occupant, ordered list, or key-dispatched cell. */
export type SlotKind = 'single' | 'list' | 'keyed';

/** Slot data context: global, current-session-optional, or strict session-bound. */
export type SlotScope = 'root' | 'session-maybe' | 'session';

/** One SlotMap entry: kind/scope axes plus the owner props the parent passes
 * at its render site. */
export interface SlotEntryDef {
  kind: SlotKind;
  scope: SlotScope;
  owner?: object;
}

/** Statically-known slot keys (the typed registration domain). */
export type SlotKey = keyof SlotMap & string;

/** Cardinality of a slot key. */
export type KindOf<K extends SlotKey> = SlotMap[K] extends { kind: infer Kind extends SlotKind }
  ? Kind
  : never;

/** Owner props share the parent supplies at the render site. */
export type OwnerOf<K extends SlotKey> = SlotMap[K] extends { owner: infer O extends object }
  ? O
  : object;

/**
 * Kind-specific registration shape: `list` requires `id` (+ display `order`),
 * `keyed` requires `key`, `single` takes neither. `priority` is the
 * shadowing rank (ascending; the lowest live entry of a cell renders).
 */
export type KindOptions<K extends SlotKey> = KindOf<K> extends 'list'
  ? { id: string; order?: number; key?: never }
  : KindOf<K> extends 'keyed'
    ? { key: string; id?: never; order?: never }
    : KindOf<K> extends 'single'
      ? { id?: never; key?: never; order?: never }
      : object;

/** Compile-time check that every merged SlotMap entry satisfies the contract. */
export type AssertSlotDefs = {
  [K in keyof SlotMap]-?: SlotMap[K] extends SlotEntryDef ? true : { error: 'not a SlotEntryDef'; key: K };
};

/** A slot component: receives the owner props of its render site. */
export type SlotComponent<K extends SlotKey> = (props: OwnerOf<K>) => ReactNode;

/** Registration record as stored by the core (type-erased component). */
export interface StoredEntry {
  component: unknown;
  /** Keyed dispatch key. */
  key?: string;
  /** List cell identity. */
  id?: string;
  /** List display order within equal priorities. */
  order?: number;
  /** Shadowing rank (ascending; lowest renders). */
  priority: number;
  /** Diagnostics: who registered. */
  registrant?: string;
  /** Registration sequence for stable ties. */
  seq: number;
}

/** Registration options for {@link register}. */
export type RegisterOptions<K extends SlotKey> = {
  /** Target slot key (the entry contributes INTO this slot). */
  name: K;
  /** Shadowing rank (default 0). */
  priority?: number;
  /** Diagnostics label of the registrant. */
  registrant?: string;
} & KindOptions<K>;

/** One slot's runtime record: never removed once created (version stays monotonic). */
interface SlotRecord {
  key: string;
  kind: SlotKind | undefined;
  scope: SlotScope | undefined;
  /** Diagnostics: which owner declared this key. */
  declaredBy: string | undefined;
  /** Monotonic declaration lifetime counter (entry mutations do not bump it). */
  declarationEpoch: number;
  entries: StoredEntry[];
  /** Frozen view of `entries` (stable reference between mutations). */
  stable: readonly StoredEntry[];
  version: number;
  listeners: Set<() => void>;
  declarationListeners: Set<() => void>;
}

const NO_ENTRIES: readonly StoredEntry[] = Object.freeze([]);

const records = new Map<string, SlotRecord>();
const dirty = new Set<SlotRecord>();
let flushScheduled = false;
let registrationSeq = 0;

function recordOf(key: string): SlotRecord {
  let rec = records.get(key);
  if (!rec) {
    rec = {
      key,
      kind: undefined,
      scope: undefined,
      declaredBy: undefined,
      declarationEpoch: 0,
      entries: [],
      stable: NO_ENTRIES,
      version: 0,
      listeners: new Set(),
      declarationListeners: new Set(),
    };
    records.set(key, rec);
  }
  return rec;
}

/** Cell identity of one entry for its slot kind (the shadowing unit). */
function cellOf(kind: SlotKind, entry: StoredEntry): string | undefined {
  if (kind === 'list') return entry.id;
  if (kind === 'keyed') return entry.key;
  return undefined; // single: the slot itself is the one cell
}

function compareEntries(kind: SlotKind) {
  return (a: StoredEntry, b: StoredEntry): number =>
    a.priority - b.priority ||
    (kind === 'list' ? (a.order ?? 0) - (b.order ?? 0) : 0) ||
    a.seq - b.seq;
}

/**
 * Declare a slot at runtime (the owner of a render site calls this; the
 * type contract lives in the SlotMap merge). The returned disposer collapses
 * the declaration: every contribution is removed and each pending
 * {@link inject} lifetime is released — re-declaring re-runs them (the slot
 * tree appears and disappears as one lifecycle unit).
 *
 * @throws when the slot is already declared (one declarer per slot).
 */
export function declareSlot(
  key: string,
  spec: { kind: SlotKind; scope: SlotScope; declaredBy?: string },
): () => void {
  const rec = recordOf(key);
  if (rec.kind !== undefined) {
    throw new Error(
      `slot "${key}" is already declared (by ${rec.declaredBy ?? 'an unknown owner'})`,
    );
  }
  rec.kind = spec.kind;
  rec.scope = spec.scope;
  rec.declaredBy = spec.declaredBy;
  rec.declarationEpoch += 1;
  markDirty(key, rec);
  notifyDeclaration(rec);
  let disposed = false;
  return () => {
    if (disposed) return;
    disposed = true;
    if (rec.kind !== spec.kind || rec.scope !== spec.scope) return; // superseded
    rec.kind = undefined;
    rec.scope = undefined;
    rec.declaredBy = undefined;
    rec.entries = [];
    rec.stable = NO_ENTRIES;
    rec.declarationEpoch += 1;
    markDirty(key, rec);
    notifyDeclaration(rec);
  };
}

/**
 * Contribute a component to a declared slot.
 *
 * Misconfiguration fails loud at this call site: registering into an
 * undeclared slot throws; a kind's required cell field (list `id`, keyed
 * `key`) missing throws; a second registration occupying the same cell at
 * the same priority throws (shadow with a different priority).
 *
 * @returns disposer removing the contribution (idempotent).
 */
export function register<K extends SlotKey>(
  options: RegisterOptions<K>,
  component: SlotComponent<K>,
): () => void {
  const rec = records.get(options.name);
  if (!rec || rec.kind === undefined || rec.scope === undefined) {
    throw new Error(
      `slot "${options.name}" is not declared (its owner must call declareSlot)`,
    );
  }
  const kind = rec.kind;
  const priority = options.priority ?? 0;
  const entry: StoredEntry = {
    component,
    priority,
    seq: ++registrationSeq,
    ...(options.registrant !== undefined ? { registrant: options.registrant } : {}),
    ...(kind === 'list' ? { id: options.id as string } : {}),
    ...(kind === 'list' && options.order !== undefined ? { order: options.order } : {}),
    ...(kind === 'keyed' ? { key: options.key as string } : {}),
  };
  if (kind === 'list' && entry.id === undefined) {
    throw new Error(`list slot "${options.name}" requires options.id`);
  }
  if (kind === 'keyed' && entry.key === undefined) {
    throw new Error(`keyed slot "${options.name}" requires options.key`);
  }
  const cell = cellOf(kind, entry);
  const occupant = rec.entries.find((e) => cellOf(kind, e) === cell && e.priority === priority);
  if (occupant) {
    const where = kind === 'single' ? '' : kind === 'list' ? ` for id "${entry.id}"` : ` for key "${entry.key}"`;
    throw new Error(
      `slot "${options.name}"${where} already has a registration at priority ${priority}` +
        `${occupant.registrant ? ` (registered by ${occupant.registrant})` : ''}` +
        ' — register at a different priority to shadow it (lowest renders)',
    );
  }
  const next = [...rec.entries, entry];
  next.sort(compareEntries(kind));
  rec.entries = next;
  rec.stable = Object.freeze(next);
  markDirty(options.name, rec);
  return () => {
    if (!rec.entries.includes(entry)) return;
    rec.entries = rec.entries.filter((e) => e !== entry);
    rec.stable = Object.freeze(rec.entries);
    markDirty(options.name, rec);
  };
}

/**
 * Run a contribution factory for each declaration lifetime of a slot
 * (dsh `slots.inject`): if the slot is already declared the callback runs
 * synchronously now; otherwise it runs on every fresh declaration. The
 * callback may return a disposer (typically its `register` call); it is
 * called when the declaration collapses or the inject itself is disposed.
 *
 * This is the timing contract that lets a plugin register into a slot whose
 * owner mounts late (or remounts) without ever importing the owner.
 */
export function inject(key: string, callback: () => (() => void) | void): () => void {
  const rec = recordOf(key);
  let live: (() => void) | undefined;
  let dead = false;
  const run = (): void => {
    if (rec.kind === undefined) return; // collapsed between the check and here
    live = callback() ?? undefined;
  };
  if (rec.kind !== undefined) run();
  const onDeclaration = (): void => {
    if (dead) return;
    if (rec.kind !== undefined) run();
    else {
      live?.();
      live = undefined;
    }
  };
  rec.declarationListeners.add(onDeclaration);
  return () => {
    rec.declarationListeners.delete(onDeclaration);
    dead = true;
    live?.();
    live = undefined;
  };
}

/**
 * Snapshot the registered entries for a key (stable reference between
 * mutations — safe as a `useSyncExternalStore` source). Empty for keys not
 * (or no longer) declared, so outlets may render ahead of registration
 * order.
 */
export function entries(key: string): readonly StoredEntry[] {
  return records.get(key)?.stable ?? NO_ENTRIES;
}

/**
 * Project a key's entries to its shadowing winners: the first live entry of
 * each cell in ledger order (single: the slot is one cell; keyed: one cell
 * per `key`; list: one cell per `id`, winners keep ledger sequence).
 * Builds a fresh array per call — a render-body read, not a stable source.
 */
export function entriesOfSlot(key: string): readonly StoredEntry[] {
  const rec = records.get(key);
  if (!rec || rec.kind === undefined) return NO_ENTRIES;
  const heads: StoredEntry[] = [];
  const seen = new Set<string | undefined>();
  for (const entry of rec.entries) {
    const cell = cellOf(rec.kind, entry);
    if (seen.has(cell)) continue;
    seen.add(cell);
    heads.push(entry);
  }
  return heads;
}

/** The winners for one keyed dispatch (entries of the slot whose `key` matches). */
export function entriesForKey(key: string, entryKey: string): readonly StoredEntry[] {
  return entriesOfSlot(key).filter((entry) => entry.key === entryKey);
}

/** Declared spec of a key, or undefined while undeclared. */
export function specOf(key: string): { kind: SlotKind; scope: SlotScope } | undefined {
  const rec = records.get(key);
  return rec?.kind && rec.scope ? { kind: rec.kind, scope: rec.scope } : undefined;
}

/** Monotonic per-key version, bumped synchronously per mutation. */
export function getVersion(key: string): number {
  return records.get(key)?.version ?? 0;
}

/** Monotonic declaration lifetime counter (0 before the first declaration). */
export function declarationEpoch(key: string): number {
  return records.get(key)?.declarationEpoch ?? 0;
}

/**
 * Subscribe to entry mutations for a key (microtask-batched). Subscribing
 * ahead of declaration is allowed.
 */
export function subscribe(key: string, listener: () => void): () => void {
  const rec = recordOf(key);
  rec.listeners.add(listener);
  return () => {
    rec.listeners.delete(listener);
  };
}

/**
 * Subscribe to declaration lifetime boundaries (synchronous).
 */
export function subscribeDeclaration(key: string, listener: () => void): () => void {
  const rec = recordOf(key);
  rec.declarationListeners.add(listener);
  return () => {
    rec.declarationListeners.delete(listener);
  };
}

function markDirty(key: string, rec: SlotRecord): void {
  rec.version += 1;
  dirty.add(rec);
  if (!flushScheduled) {
    flushScheduled = true;
    queueMicrotask(flush);
  }
}

function flush(): void {
  // Reset before iterating so a listener mutating the registry re-schedules.
  flushScheduled = false;
  const pending = [...dirty];
  dirty.clear();
  for (const rec of pending) {
    for (const listener of [...rec.listeners]) listener();
  }
}

/** Test/host seam: drop every declaration and contribution (registry state
 * is process-global; suites that mount surfaces reset between cases). */
export function __resetSlotsForTests(): void {
  records.clear();
  dirty.clear();
  flushScheduled = false;
}
