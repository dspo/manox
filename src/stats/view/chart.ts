//! 折线图几何（对齐 Rust view.rs 的图表辅助函数）。

import { dateOffset } from "../date.ts";
import type { Period, UsageRecord } from "../types.ts";

export const STEP_CHART_MAX_WIDTH = 78;
export const STEP_CHART_HEIGHT = 17;
export const Y_TICK_COUNT = 10;
export const X_TICK_MIN_COUNT = 6;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export function fixedChartArea(area: Rect): Rect {
  return {
    x: area.x,
    y: area.y,
    width: Math.min(area.width, STEP_CHART_MAX_WIDTH),
    height: Math.min(area.height, STEP_CHART_HEIGHT),
  };
}

export function chartDateRange(
  period: Period,
  today: string,
  records: UsageRecord[],
): [string, string] | undefined {
  switch (period) {
    case "last7":
      return [dateOffset(today, -6), today];
    case "last30":
      return [dateOffset(today, -29), today];
    case "all": {
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
  }
}

export function yTickValues(maxY: number, tickCount: number): number[] {
  if (tickCount <= 1) return [0];
  const m = Math.max(maxY, 0);
  const out: number[] = [];
  for (let i = 0; i < tickCount; i++) {
    out.push((m * i) / (tickCount - 1));
  }
  return out;
}

export function xTickIndices(dayCount: number): number[] {
  if (dayCount === 0) return [];
  if (dayCount >= 1 && dayCount <= 7) {
    return Array.from({ length: dayCount }, (_, i) => i);
  }
  const tickCount = Math.min(X_TICK_MIN_COUNT, dayCount);
  const last = dayCount - 1;
  const indices: number[] = [];
  for (let i = 0; i < tickCount; i++) {
    const numerator = i * last + Math.floor((tickCount - 1) / 2);
    indices.push(Math.floor(numerator / (tickCount - 1)));
  }
  return dedupConsecutive(indices);
}

function dedupConsecutive(arr: number[]): number[] {
  const out: number[] = [];
  for (const v of arr) {
    if (out.length === 0 || out[out.length - 1] !== v) out.push(v);
  }
  return out;
}

export function chartXBoundary(
  idx: number,
  dayCount: number,
  plotLeft: number,
  plotRight: number,
): number {
  if (dayCount === 0 || plotLeft >= plotRight) return plotLeft;
  const width = plotRight - plotLeft;
  const offset = Math.floor(
    (Math.min(idx, dayCount) * width + Math.floor(dayCount / 2)) / dayCount,
  );
  return plotLeft + Math.min(offset, width);
}

export function plotRightBeforeLegend(
  plotLeft: number,
  availablePlotRight: number,
  legendWidth: number,
  legendGap: number,
): number {
  if (plotLeft >= availablePlotRight) return plotLeft;
  const reserved = legendWidth + legendGap;
  const reservedRight = Math.max(0, availablePlotRight - reserved);
  return Math.max(reservedRight, plotLeft + 1);
}

export function chartLegendMaxWidth(names: string[]): number {
  let max = 1;
  for (const name of names) {
    max = Math.max(max, Array.from(name).length + 2);
  }
  return max;
}

export function valueRow(
  value: number,
  maxBound: number,
  plotTop: number,
  plotBottom: number,
): number {
  const height = Math.max(0, plotBottom - plotTop);
  const ratio = Math.max(0, Math.min(1, value / maxBound));
  return plotBottom - Math.round(ratio * height);
}

export function roundedTransitionCorners(
  fromY: number,
  toY: number,
): [string, string] {
  return toY < fromY ? ["╯", "╭"] : ["╮", "╰"];
}
