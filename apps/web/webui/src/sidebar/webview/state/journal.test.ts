/**
 * Conformance tests for the TS `JournalStream` twin (T3). Loads the same
 * shared vectors as the Rust side
 * (`crates/manox-protocol/test-vectors/journal-cases.json`) and asserts
 * byte-identical publish sequences and protocol-failure messages, making
 * cross-engine equivalence a hard, mechanically checked guarantee.
 *
 * The JSON import resolves through `tsconfig.json`'s `resolveJsonModule`;
 * vitest (vite) parses the same path natively at runtime.
 */

import { describe, expect, it } from "vitest";

import vectors from "../../../../../../../crates/manox-protocol/test-vectors/journal-cases.json";
import {
	type JournalChange,
	type JournalEntry,
	JournalStream,
	type JournalSource,
} from "./journal";

/** A vector entry item: a bare seq or a `{first,last}` span. */
type EntryItem = number | { first: number; last: number };

function toEntry(item: EntryItem): JournalEntry {
	return typeof item === "number" ? { first: item, last: item } : { first: item.first, last: item.last };
}

function toEntries(items: readonly EntryItem[]): JournalEntry[] {
	return items.map(toEntry);
}

/** Normalize a change for JSON comparison (drop `undefined` heads/tails). */
function normalizeChange(change: JournalChange): Record<string, unknown> {
	switch (change.type) {
		case "replace":
		case "prepend":
			return {
				type: change.type,
				entries: change.entries.map((entry) => [entry.first, entry.last]),
				hasMore: change.hasMore,
			};
		case "append":
			return { type: "append", entries: [[change.entry.first, change.entry.last]] };
	}
}

interface VectorEvent {
	readonly kind: "opened" | "entry" | "prepend" | "generation" | "gap-repair" | "journal";
	readonly cursor?: number;
	readonly entries?: EntryItem[];
	readonly seq?: number;
	readonly first?: number;
	readonly last?: number;
	readonly hasMore?: boolean;
	readonly through?: number;
}

interface PublishSpec {
	readonly type: "replace" | "prepend" | "append";
	readonly entries?: EntryItem[];
	readonly seq?: number;
	readonly first?: number;
	readonly last?: number;
	readonly hasMore?: boolean;
}

interface VectorCase {
	readonly name: string;
	readonly events: VectorEvent[];
	readonly expectedPublish: PublishSpec[];
	readonly expectedFail?: string;
	readonly expectedCursors?: {
		readonly first: number | null;
		readonly last: number | null;
		readonly resume: number | null;
	};
}

const cases = vectors as unknown as VectorCase[];

function entryOf(event: VectorEvent): JournalEntry {
	if (event.seq !== undefined && event.first === undefined && event.last === undefined) {
		return { first: event.seq, last: event.seq };
	}
	if (event.first !== undefined && event.last !== undefined) {
		return { first: event.first, last: event.last };
	}
	throw new Error(`vector entry event must carry \`seq\` or \`first\`+\`last\`: ${JSON.stringify(event)}`);
}

function expectedOf(spec: PublishSpec): Record<string, unknown> {
	if (spec.type === "append") {
		const entry = spec.seq !== undefined
			? { first: spec.seq, last: spec.seq }
			: { first: spec.first as number, last: spec.last as number };
		return { type: "append", entries: [[entry.first, entry.last]] };
	}
	return {
		type: spec.type,
		entries: toEntries(spec.entries ?? []).map((entry) => [entry.first, entry.last]),
		hasMore: spec.hasMore ?? false,
	};
}

/**
 * Fake `PageHistory`: serves explicit `gap-repair` pages (per `through`,
 * consumed in declaration order), falling back to a sparse journal.
 */
class VectorSource implements JournalSource {
	readonly name = "journal";
	private readonly overrides = new Map<number, JournalEntry[][]>();
	private readonly journal = new Map<number, JournalEntry>();

	constructor(events: VectorEvent[]) {
		for (const event of events) {
			if (event.kind === "gap-repair") {
				const queue = this.overrides.get(event.through as number) ?? [];
				queue.push(toEntries(event.entries ?? []));
				this.overrides.set(event.through as number, queue);
			} else if (event.kind === "journal") {
				for (const item of event.entries ?? []) {
					const entry = toEntry(item);
					this.journal.set(entry.first, entry);
				}
			}
		}
	}

	readPage(through: number): JournalEntry[] {
		const queue = this.overrides.get(through);
		if (queue !== undefined && queue.length > 0) {
			return queue.shift() as JournalEntry[];
		}
		const page: JournalEntry[] = [];
		for (let seq = 0; seq <= through; seq += 1) {
			const entry = this.journal.get(seq);
			if (entry !== undefined) {
				page.push(entry);
			}
		}
		return page;
	}
}

describe("JournalStream shared conformance vectors (webui ⇄ manox-protocol)", () => {
	it("loads a non-empty vector file", () => {
		expect(cases.length).toBeGreaterThan(0);
	});

	for (const testCase of cases) {
		it(testCase.name, () => {
			// `apply` reports the violation through the `failed` callback (once)
			// and returns the message; the loop breaks on the first violation.
			const publishes: JournalChange[] = [];
			const failures: string[] = [];
			const stream = new JournalStream(new VectorSource(testCase.events), {
				publish: (change) => publishes.push(change),
				failed: (message) => failures.push(message),
			});
			for (const event of testCase.events) {
				let failure: string | undefined;
				switch (event.kind) {
					case "opened":
						failure = stream.opened(event.cursor as number, toEntries(event.entries ?? []));
						break;
					case "entry":
						failure = stream.entry(entryOf(event));
						break;
					case "prepend":
						failure = stream.prependPage(toEntries(event.entries ?? []), event.hasMore ?? false);
						break;
					case "generation":
						failure = stream.restart();
						break;
					case "gap-repair":
					case "journal":
						continue;
				}
				if (failure !== undefined) {
					break;
				}
			}
			expect(publishes.map(normalizeChange)).toEqual(testCase.expectedPublish.map(expectedOf));
			if (testCase.expectedFail !== undefined) {
				expect(failures[0]).toBe(testCase.expectedFail);
			} else {
				expect(failures).toEqual([]);
			}
			if (testCase.expectedCursors !== undefined) {
				expect(stream.cursors()).toEqual({
					first: testCase.expectedCursors.first ?? undefined,
					last: testCase.expectedCursors.last ?? undefined,
					resume: testCase.expectedCursors.resume ?? undefined,
				});
			}
		});
	}
});
