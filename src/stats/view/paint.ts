//! 将折线图 / 竞速帧绘制到字符网格（对齐 Rust view.rs 的 draw_* 逐格绘制）。

import { dateOffset, daysDiff } from "../date.ts";
import { formatTokens, shortDate } from "../format.ts";
import { topModelsCovering, totalsByModel } from "../aggregate.ts";
import {
  type Period,
  type UsageRecord,
  type UsageTotals,
  emptyTotals,
  totalTokens,
} from "../types.ts";
import {
  type Color,
  COLOR_DARK_GRAY,
  COLOR_LIGHT_YELLOW,
  COLOR_WHITE,
  PALETTE,
  fadeColor,
  smoothstep,
  toHex,
} from "./colors.ts";
import {
  STEP_CHART_HEIGHT,
  STEP_CHART_MAX_WIDTH,
  Y_TICK_COUNT,
  chartDateRange,
  chartLegendMaxWidth,
  chartXBoundary,
  plotRightBeforeLegend,
  roundedTransitionCorners,
  valueRow,
  xTickIndices,
  yTickValues,
} from "./chart.ts";
import {
  type RaceFrame,
  currentRaceFrame,
  interpolateUsageTotals,
  nearestFreeRow,
  pacmanClearX,
  pacmanRowProgress,
  pacmanSymbol,
  raceClearProgress,
  raceMaxValue,
  raceRankMap,
  raceUsageMap,
  RACE_VISIBLE_MODELS,
} from "./race.ts";
import { Grid, type Row } from "./grid.ts";
import { truncateText, textWidth, usageCellText } from "./table.ts";

function chartWidth(areaWidth: number): number {
  return Math.min(areaWidth, STEP_CHART_MAX_WIDTH);
}

// ══════════════════════════════════════════════════
// 阶梯折线图
// ══════════════════════════════════════════════════

export function buildStepChart(
  records: UsageRecord[],
  period: Period,
  today: string,
  periodLabelText: string,
  areaWidth: number,
): Row[] {
  if (records.length === 0) {
    return [
      [
        {
          text: "  No data in selected period.",
          color: toHex(COLOR_DARK_GRAY),
        },
      ],
    ];
  }

  const totals = totalsByModel(records);
  const topModels = topModelsCovering(totals, 0.8);
  const range = chartDateRange(period, today, records);
  if (!range) return [[]];
  const [minDate, maxDate] = range;
  const dayCount = Math.max(
    1,
    Math.max(0, daysDiff(minDate, maxDate) ?? 0) + 1,
  );

  const series = new Map<string, number[]>();
  for (const m of topModels) series.set(m, new Array(dayCount).fill(0));
  for (const r of records) {
    const idx = Math.max(0, daysDiff(minDate, r.date) ?? 0);
    if (idx >= dayCount) continue;
    const arr = series.get(r.model);
    if (arr) arr[idx]! += r.in_tokens + r.out_tokens;
  }

  let maxY = 1;
  const chartSeries: { model: string; values: number[]; color: Color }[] = [];
  topModels.forEach((model, idx) => {
    const color = PALETTE[idx % PALETTE.length]!;
    const values = series.get(model) ?? [];
    for (const y of values) if (y > maxY) maxY = y;
    chartSeries.push({ model, values, color });
  });

  const width = chartWidth(areaWidth);
  const height = STEP_CHART_HEIGHT;
  if (width < 24 || height < 6) {
    return [[{ text: `Tokens per Day · ${periodLabelText}`, bold: true }]];
  }
  const grid = new Grid(width, height);

  // title（row 0）
  grid.setString(0, 0, ` Tokens per Day · ${periodLabelText} `, {
    color: COLOR_WHITE,
    bold: true,
  });

  const yTicks = yTickValues(maxY, Y_TICK_COUNT);
  const yLabels = yTicks.map((v) => formatTokens(Math.round(v)));
  const labelWidth = Math.max(4, ...yLabels.map((l) => textWidth(l)));

  const axisX = labelWidth;
  const plotLeft = axisX + 1;
  const availableRight = width - 1;
  const legendWidth = Math.max(
    1,
    Math.min(
      chartLegendMaxWidth(chartSeries.map((s) => s.model)),
      Math.max(1, availableRight - plotLeft + 1),
    ),
  );
  const legendGap = 2;
  const plotRight = plotRightBeforeLegend(
    plotLeft,
    availableRight,
    legendWidth,
    legendGap,
  );
  const plotTop = 2;
  const plotBottom = height - 2;
  if (plotLeft >= plotRight || plotTop >= plotBottom) {
    return grid.toRows();
  }

  const maxBound = Math.max(maxY * 1.05, 1);

  const usedYRows = new Set<number>();
  for (let i = 0; i < yTicks.length; i++) {
    const y = valueRow(yTicks[i]!, maxBound, plotTop, plotBottom);
    if (!usedYRows.has(y)) {
      usedYRows.add(y);
      rightAlignedLabel(grid, 0, y, labelWidth, yLabels[i]!, COLOR_DARK_GRAY);
    }
  }

  for (let y = plotTop; y <= plotBottom; y++) {
    grid.setChar(axisX, y, "│", { color: COLOR_DARK_GRAY });
  }

  const occupied = new Set<string>();
  const plotArea = {
    x: plotLeft,
    y: plotTop,
    right: plotRight + 1,
    bottom: plotBottom + 1,
  };
  for (const s of chartSeries) {
    drawRoundedStepSeries(
      grid,
      plotArea,
      dayCount,
      maxBound,
      s.values,
      s.color,
      occupied,
    );
  }

  drawXTickLabels(
    grid,
    minDate,
    maxDate,
    dayCount,
    plotLeft,
    plotRight,
    plotBottom + 1,
  );

  // legend
  const legendX = availableRight + 1 - legendWidth;
  const legendHeight = Math.min(chartSeries.length, plotBottom - plotTop + 1);
  for (let i = 0; i < legendHeight; i++) {
    const s = chartSeries[i]!;
    grid.setString(legendX, plotTop + i, `● ${s.model}`, { color: s.color });
  }

  return grid.toRows();
}

function rightAlignedLabel(
  grid: Grid,
  x: number,
  y: number,
  width: number,
  label: string,
  color: Color,
): void {
  const labelX = x + Math.max(0, width - textWidth(label));
  grid.setString(labelX, y, label, { color });
}

interface PlotArea {
  x: number;
  y: number;
  right: number;
  bottom: number;
}

function drawRoundedStepSeries(
  grid: Grid,
  plotArea: PlotArea,
  dayCount: number,
  maxBound: number,
  values: number[],
  color: Color,
  occupied: Set<string>,
): void {
  if (values.length === 0 || dayCount === 0) return;
  const plotLeft = plotArea.x;
  const plotRight = plotArea.right - 1;
  const plotTop = plotArea.y;
  const plotBottom = plotArea.bottom - 1;
  const style = { color };
  const rows = values.map((v) => valueRow(v, maxBound, plotTop, plotBottom));

  for (let idx = 0; idx < values.length; idx++) {
    const x0 = chartXBoundary(idx, dayCount, plotLeft, plotRight);
    const x1 = chartXBoundary(idx + 1, dayCount, plotLeft, plotRight);
    const y = rows[idx]!;
    const nextY = rows[idx + 1];
    const changesNext = nextY !== undefined && nextY !== y;
    const end = changesNext ? x1 - 1 : x1;
    drawHorizontal(grid, x0, end, y, style, occupied);
    if (nextY !== undefined && nextY !== y) {
      drawRoundedTransition(grid, x1, y, nextY, style, occupied);
    }
  }
}

function drawHorizontal(
  grid: Grid,
  start: number,
  end: number,
  y: number,
  style: { color: Color },
  occupied: Set<string>,
): void {
  if (start > end) return;
  for (let x = start; x <= end; x++)
    setChartSymbol(grid, x, y, "─", style, occupied);
}

function drawRoundedTransition(
  grid: Grid,
  x: number,
  fromY: number,
  toY: number,
  style: { color: Color },
  occupied: Set<string>,
): void {
  const [fromCorner, toCorner] = roundedTransitionCorners(fromY, toY);
  setChartSymbol(grid, x, fromY, fromCorner, style, occupied);
  setChartSymbol(grid, x, toY, toCorner, style, occupied);
  const start = Math.min(fromY, toY) + 1;
  const end = Math.max(fromY, toY) - 1;
  for (let y = start; y <= end; y++)
    setChartSymbol(grid, x, y, "│", style, occupied);
}

function setChartSymbol(
  grid: Grid,
  x: number,
  y: number,
  symbol: string,
  style: { color: Color },
  occupied: Set<string>,
): void {
  const key = `${x},${y}`;
  if (occupied.has(key)) return;
  if (x < 0 || y < 0 || x >= grid.width || y >= grid.height) return;
  occupied.add(key);
  grid.setChar(x, y, symbol, style);
}

function drawXTickLabels(
  grid: Grid,
  minDate: string,
  maxDate: string,
  dayCount: number,
  plotLeft: number,
  plotRight: number,
  y: number,
): void {
  const occupiedCols = new Set<number>();
  for (const idx of xTickIndices(dayCount)) {
    const date = idx + 1 === dayCount ? maxDate : safeDateOffset(minDate, idx);
    const label = shortDate(date);
    const labelWidth = textWidth(label);
    const tickX = chartXBoundary(
      idx,
      Math.max(dayCount, 1),
      plotLeft,
      plotRight,
    );
    const labelX = Math.min(
      Math.max(tickX - Math.floor(labelWidth / 2), plotLeft),
      plotRight - Math.max(0, labelWidth - 1),
    );
    const labelEnd = labelX + Math.max(0, labelWidth - 1);
    let overlap = false;
    for (let x = labelX; x <= labelEnd; x++)
      if (occupiedCols.has(x)) overlap = true;
    if (overlap) continue;
    for (let x = labelX; x <= labelEnd; x++) occupiedCols.add(x);
    grid.setString(labelX, y, label, { color: COLOR_DARK_GRAY });
  }
}

function safeDateOffset(min: string, idx: number): string {
  try {
    return dateOffset(min, idx);
  } catch {
    return "";
  }
}

// ══════════════════════════════════════════════════
// 柱状竞速帧
// ══════════════════════════════════════════════════

export function buildRaceFrame(
  frames: RaceFrame[],
  raceTick: number,
  areaWidth: number,
): Row[] {
  const width = chartWidth(areaWidth);
  const height = STEP_CHART_HEIGHT;
  if (width < 32 || height < 6) {
    return [[{ text: "Model Tokens Top 15 · All time" }]];
  }
  if (frames.length === 0) {
    return [
      [
        {
          text: "  No data for bar chart race.",
          color: toHex(COLOR_DARK_GRAY),
        },
      ],
    ];
  }

  const cur = currentRaceFrame(raceTick, frames);
  if (!cur) return [[]];
  const [previous, current, tween] = cur;
  const clearProgress = raceClearProgress(raceTick, frames.length);
  const maxValue = raceMaxValue(frames);

  const grid = new Grid(width, height);
  drawRaceFrame(
    grid,
    width,
    height,
    previous,
    current,
    tween,
    raceTick,
    clearProgress,
    maxValue,
  );
  return grid.toRows();
}

function drawRaceFrame(
  grid: Grid,
  width: number,
  height: number,
  previous: RaceFrame,
  current: RaceFrame,
  tween: number,
  tick: number,
  clearProgress: number,
  maxValue: number,
): void {
  // title
  grid.setString(0, 0, " Model Tokens Top 15 · All time ", {
    color: fadeColor(COLOR_WHITE, clearProgress),
    bold: true,
  });
  grid.setString(
    textWidth(" Model Tokens Top 15 · All time "),
    0,
    shortDate(current.date),
    {
      color: fadeColor(COLOR_LIGHT_YELLOW, clearProgress),
      bold: true,
    },
  );

  if (current.entries.length === 0) {
    grid.setString(0, 2, "  Waiting for the first model token usage...", {
      color: fadeColor(COLOR_DARK_GRAY, clearProgress),
    });
    return;
  }

  const rowCount = Math.min(
    RACE_VISIBLE_MODELS,
    current.entries.length,
    height - 2,
  );
  if (rowCount === 0) return;

  const visible = current.entries.slice(0, rowCount);
  const modelWidth = clamp(
    Math.max(10, ...visible.map((e) => textWidth(e.model))),
    10,
    22,
  );
  const valueWidth = Math.max(
    4,
    ...visible.map((e) => textWidth(usageCellText(e.usage))),
  );

  const barLeft = modelWidth + 2;
  const barRight = Math.max(0, width - valueWidth - 3);
  if (barLeft >= barRight) return;

  const plotTop = 2;
  const plotBottom = plotTop + rowCount - 1;
  const previousRanks = raceRankMap(previous);
  const previousUsages = raceUsageMap(previous);
  const eased = smoothstep(tween);
  const barWidth = barRight - barLeft + 1;
  const occupiedRows = new Set<number>();

  visible.forEach((entry, rank) => {
    const prevRank = Math.min(
      previousRanks.get(entry.model) ?? rowCount,
      rowCount,
    );
    const interpolatedRank = prevRank + (rank - prevRank) * eased;
    const candidateRow = plotTop + Math.round(interpolatedRank);
    const row = nearestFreeRow(candidateRow, plotTop, plotBottom, occupiedRows);
    if (row === undefined) return;
    occupiedRows.add(row);

    const prevUsage = previousUsages.get(entry.model) ?? emptyTotals();
    const usage = interpolateUsageTotals(prevUsage, entry.usage, eased);
    const total = totalTokens(usage);
    const barLen = Math.max(
      total > 0 ? 1 : 0,
      Math.round((total / Math.max(1, maxValue)) * barWidth),
    );
    const label = truncateText(entry.model, modelWidth);
    const valueLabel = usageCellText(usage);

    let clearX: number | undefined;
    if (clearProgress > Number.EPSILON) {
      const lineRight = Math.min(barRight + 2 + valueWidth - 1, width - 1);
      const rowProgress = pacmanRowProgress(clearProgress, rank, rowCount);
      clearX = pacmanClearX(0, lineRight, rowProgress);
    }

    drawAfterClearX(
      grid,
      0,
      row,
      label,
      { color: entry.color, bold: true },
      clearX,
    );

    if (barLen > 0) {
      const barStart =
        clearX !== undefined ? Math.max(barLeft, clearX + 1) : barLeft;
      const barEnd = barLeft + Math.min(barLen, barWidth) - 1;
      if (barStart <= barEnd) {
        grid.setString(barStart, row, "█".repeat(barEnd - barStart + 1), {
          color: entry.color,
        });
      }
    }

    drawAfterClearX(
      grid,
      barRight + 2,
      row,
      valueLabel,
      { color: fadeColor(COLOR_DARK_GRAY, clearProgress) },
      clearX,
    );

    if (clearX !== undefined) {
      grid.setChar(clearX, row, pacmanSymbol(tick, rank), {
        color: COLOR_LIGHT_YELLOW,
        bold: true,
      });
    }
  });
}

function drawAfterClearX(
  grid: Grid,
  x: number,
  y: number,
  text: string,
  style: { color: Color; bold?: boolean },
  clearX: number | undefined,
): void {
  if (clearX === undefined) {
    grid.setString(x, y, text, style);
    return;
  }
  const chars = Array.from(text);
  let visibleX: number | undefined;
  let visible = "";
  for (let offset = 0; offset < chars.length; offset++) {
    const charX = x + offset;
    if (charX > clearX) {
      if (visibleX === undefined) visibleX = charX;
      visible += chars[offset];
    }
  }
  if (visible.length > 0 && visibleX !== undefined) {
    grid.setString(visibleX, y, visible, style);
  }
}

function clamp(value: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, value));
}
