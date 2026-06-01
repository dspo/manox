//! 柱状竞速计算（对齐 Rust view.rs 的 race_* 与插值逻辑）。

import { dateOffset, daysDiff } from "../date.ts";
import { totalsByModel } from "../aggregate.ts";
import {
  type UsageRecord,
  type UsageTotals,
  cloneTotals,
  emptyTotals,
  totalTokens,
} from "../types.ts";
import { type Color, PALETTE, interpolateU64 } from "./colors.ts";

export const RACE_VISIBLE_MODELS = 15;
export const RACE_TWEEN_STEPS = 12;
export const RACE_FINAL_HOLD_TICKS = RACE_TWEEN_STEPS * 3;
export const RACE_FINAL_CLEAR_TICKS = RACE_TWEEN_STEPS * 4;

export interface RaceEntry {
  model: string;
  value: number;
  usage: UsageTotals;
  color: Color;
}

export interface RaceFrame {
  date: string;
  entries: RaceEntry[];
  /** key = `${agent}\t${model}` */
  cells: Map<string, UsageTotals>;
}

export function allTimeDateRange(
  records: UsageRecord[],
): [string, string] | undefined {
  const first = records[0];
  if (!first) return undefined;
  let min = first.date;
  let max = first.date;
  for (let i = 1; i < records.length; i++) {
    const d = records[i]!.date;
    if (d < min) min = d;
    if (d > max) max = d;
  }
  return [min, max];
}

export function raceColorMap(records: UsageRecord[]): Map<string, Color> {
  const totals = totalsByModel(records);
  const models = [...totals.entries()].map(
    ([model, usage]) => [model, totalTokens(usage)] as [string, number],
  );
  models.sort(
    (a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0),
  );
  const map = new Map<string, Color>();
  models.forEach(([model], idx) => {
    map.set(model, PALETTE[idx % PALETTE.length]!);
  });
  return map;
}

function cloneCells(cells: Map<string, UsageTotals>): Map<string, UsageTotals> {
  const out = new Map<string, UsageTotals>();
  for (const [k, v] of cells) out.set(k, cloneTotals(v));
  return out;
}

export function addUsageTotals(target: UsageTotals, usage: UsageTotals): void {
  target.in_tokens += usage.in_tokens;
  target.total_tokens += usage.total_tokens;
  target.out_tokens += usage.out_tokens;
  target.cache_read_input_tokens += usage.cache_read_input_tokens;
  target.cache_creation_input_tokens += usage.cache_creation_input_tokens;
}

export function interpolateUsageTotals(
  from: UsageTotals,
  to: UsageTotals,
  tween: number,
): UsageTotals {
  return {
    in_tokens: interpolateU64(from.in_tokens, to.in_tokens, tween),
    total_tokens: interpolateU64(from.total_tokens, to.total_tokens, tween),
    out_tokens: interpolateU64(from.out_tokens, to.out_tokens, tween),
    cache_read_input_tokens: interpolateU64(
      from.cache_read_input_tokens,
      to.cache_read_input_tokens,
      tween,
    ),
    cache_creation_input_tokens: interpolateU64(
      from.cache_creation_input_tokens,
      to.cache_creation_input_tokens,
      tween,
    ),
  };
}

export function interpolateUsageCells(
  prev: Map<string, UsageTotals>,
  next: Map<string, UsageTotals>,
  tween: number,
): Map<string, UsageTotals> {
  const keys = new Set<string>([...prev.keys(), ...next.keys()]);
  const out = new Map<string, UsageTotals>();
  for (const key of keys) {
    const from = prev.get(key) ?? emptyTotals();
    const to = next.get(key) ?? emptyTotals();
    out.set(key, interpolateUsageTotals(from, to, tween));
  }
  return out;
}

export function totalsByModelFromCells(
  cells: Map<string, UsageTotals>,
): Map<string, UsageTotals> {
  const totals = new Map<string, UsageTotals>();
  for (const [key, usage] of cells) {
    const model = key.split("\t")[1]!;
    let t = totals.get(model);
    if (!t) {
      t = emptyTotals();
      totals.set(model, t);
    }
    addUsageTotals(t, usage);
  }
  return totals;
}

export function raceEntries(
  totals: Map<string, UsageTotals>,
  colorMap: Map<string, Color>,
): RaceEntry[] {
  const entries: RaceEntry[] = [];
  for (const [model, usage] of totals) {
    if (totalTokens(usage) <= 0) continue;
    entries.push({
      color: colorMap.get(model) ?? [255, 255, 255],
      model,
      value: totalTokens(usage),
      usage,
    });
  }
  entries.sort(
    (a, b) =>
      b.value - a.value || (a.model < b.model ? -1 : a.model > b.model ? 1 : 0),
  );
  return entries.slice(0, RACE_VISIBLE_MODELS);
}

function dateForDay(
  minDate: string,
  maxDate: string,
  dayIdx: number,
  dayCount: number,
): string {
  if (dayIdx + 1 === dayCount) return maxDate;
  try {
    return dateOffset(minDate, dayIdx);
  } catch {
    return minDate;
  }
}

export function interpolatedRaceCells(
  dayIdx: number,
  snapshots: [number, Map<string, UsageTotals>][],
): Map<string, UsageTotals> {
  const firstSnap = snapshots[0];
  if (!firstSnap) return new Map();
  if (dayIdx <= firstSnap[0]) return cloneCells(firstSnap[1]);

  for (let i = 0; i + 1 < snapshots.length; i++) {
    const [prevIdx, prevValues] = snapshots[i]!;
    const [nextIdx, nextValues] = snapshots[i + 1]!;
    if (dayIdx === prevIdx) return cloneCells(prevValues);
    if (dayIdx >= prevIdx && dayIdx <= nextIdx) {
      if (dayIdx === nextIdx) return cloneCells(nextValues);
      const span = Math.max(1, nextIdx - prevIdx);
      const tween = (dayIdx - prevIdx) / span;
      return interpolateUsageCells(prevValues, nextValues, tween);
    }
  }

  const last = snapshots[snapshots.length - 1];
  return last ? cloneCells(last[1]) : new Map();
}

export function raceFrames(records: UsageRecord[]): RaceFrame[] {
  const range = allTimeDateRange(records);
  if (!range) return [];
  const [minDate, maxDate] = range;
  const dayCount = Math.max(0, daysDiff(minDate, maxDate) ?? 0) + 1;
  const colorMap = raceColorMap(records);

  const deltasByDate = new Map<string, Map<string, UsageTotals>>();
  for (const record of records) {
    let dayMap = deltasByDate.get(record.date);
    if (!dayMap) {
      dayMap = new Map();
      deltasByDate.set(record.date, dayMap);
    }
    const key = `${record.agent}\t${record.model}`;
    let t = dayMap.get(key);
    if (!t) {
      t = emptyTotals();
      dayMap.set(key, t);
    }
    t.in_tokens += record.in_tokens;
    t.total_tokens += record.total_tokens;
    t.out_tokens += record.out_tokens;
    t.cache_read_input_tokens += record.cache_read_input_tokens;
    t.cache_creation_input_tokens += record.cache_creation_input_tokens;
  }

  const cumulative = new Map<string, UsageTotals>();
  const snapshots: [number, Map<string, UsageTotals>][] = [];
  for (let dayIdx = 0; dayIdx < dayCount; dayIdx++) {
    const date = dateForDay(minDate, maxDate, dayIdx, dayCount);
    const deltas = deltasByDate.get(date);
    if (deltas) {
      for (const [key, usage] of deltas) {
        let t = cumulative.get(key);
        if (!t) {
          t = emptyTotals();
          cumulative.set(key, t);
        }
        addUsageTotals(t, usage);
      }
      snapshots.push([dayIdx, cloneCells(cumulative)]);
    }
  }

  const frames: RaceFrame[] = [];
  for (let dayIdx = 0; dayIdx < dayCount; dayIdx++) {
    const date = dateForDay(minDate, maxDate, dayIdx, dayCount);
    const cells = interpolatedRaceCells(dayIdx, snapshots);
    const totals = totalsByModelFromCells(cells);
    const entries = raceEntries(totals, colorMap);
    frames.push({ date, entries, cells });
  }
  return frames;
}

export function raceMaxValue(frames: RaceFrame[]): number {
  let max = 1;
  for (const frame of frames) {
    for (const entry of frame.entries) {
      if (entry.value > max) max = entry.value;
    }
  }
  return Math.max(max, 1);
}

export function raceCycleTick(tick: number, frameCount: number): number {
  if (frameCount === 0) return 0;
  const frameTicks = frameCount * RACE_TWEEN_STEPS;
  const cycleTicks =
    frameTicks + RACE_FINAL_HOLD_TICKS + RACE_FINAL_CLEAR_TICKS;
  return tick % cycleTicks;
}

export function raceFrameIndex(tick: number, frameCount: number): number {
  if (frameCount === 0) return 0;
  const cycleTick = raceCycleTick(tick, frameCount);
  const frameTicks = frameCount * RACE_TWEEN_STEPS;
  if (cycleTick >= frameTicks) return frameCount - 1;
  return Math.floor(cycleTick / RACE_TWEEN_STEPS);
}

export function raceTween(tick: number, frameCount: number): number {
  if (frameCount === 0) return 0;
  const cycleTick = raceCycleTick(tick, frameCount);
  const frameTicks = frameCount * RACE_TWEEN_STEPS;
  if (cycleTick >= frameTicks) return 1;
  return (cycleTick % RACE_TWEEN_STEPS) / RACE_TWEEN_STEPS;
}

export function raceClearProgress(tick: number, frameCount: number): number {
  if (frameCount === 0) return 0;
  const cycleTick = raceCycleTick(tick, frameCount);
  const clearStart = frameCount * RACE_TWEEN_STEPS + RACE_FINAL_HOLD_TICKS;
  if (cycleTick < clearStart) return 0;
  const progress = Math.max(
    0,
    Math.min(1, (cycleTick - clearStart + 1) / RACE_FINAL_CLEAR_TICKS),
  );
  return smoothstepLocal(progress);
}

function smoothstepLocal(value: number): number {
  const v = Math.max(0, Math.min(1, value));
  return v * v * (3 - 2 * v);
}

export function pacmanRowProgress(
  clearProgress: number,
  rank: number,
  rowCount: number,
): number {
  if (rowCount <= 1) return Math.max(0, Math.min(1, clearProgress));
  const stagger = (rank / (rowCount - 1)) * 0.18;
  return Math.max(0, Math.min(1, (clearProgress - stagger) / 0.82));
}

export function pacmanClearX(
  left: number,
  right: number,
  progress: number,
): number {
  if (left >= right) return left;
  const width = right - left;
  return left + Math.round(width * Math.max(0, Math.min(1, progress)));
}

export function pacmanSymbol(tick: number, rank: number): string {
  const phase = (Math.floor(tick / 2) + rank) % 2;
  return phase === 0 ? "ᗧ" : "●";
}

export function raceRankMap(frame: RaceFrame): Map<string, number> {
  const map = new Map<string, number>();
  frame.entries.forEach((entry, rank) => map.set(entry.model, rank));
  return map;
}

export function raceUsageMap(frame: RaceFrame): Map<string, UsageTotals> {
  const map = new Map<string, UsageTotals>();
  for (const entry of frame.entries) map.set(entry.model, entry.usage);
  return map;
}

export function nearestFreeRow(
  candidate: number,
  top: number,
  bottom: number,
  occupied: Set<number>,
): number | undefined {
  if (top > bottom) return undefined;
  const c = Math.max(top, Math.min(bottom, candidate));
  if (!occupied.has(c)) return c;
  const maxDistance = bottom - top;
  for (let distance = 1; distance <= maxDistance; distance++) {
    const up = c - distance;
    if (up >= top && !occupied.has(up)) return up;
    const down = c + distance;
    if (down <= bottom && !occupied.has(down)) return down;
  }
  return undefined;
}

export function currentRaceFrame(
  tick: number,
  frames: RaceFrame[],
): [RaceFrame, RaceFrame, number] | undefined {
  if (frames.length === 0) return undefined;
  const currentIdx = raceFrameIndex(tick, frames.length);
  const previousIdx = Math.max(0, currentIdx - 1);
  return [
    frames[previousIdx]!,
    frames[currentIdx]!,
    raceTween(tick, frames.length),
  ];
}
