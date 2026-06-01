import { test, expect, describe } from "bun:test";

import {
  fixedChartArea,
  chartDateRange,
  chartXBoundary,
  plotRightBeforeLegend,
  yTickValues,
  xTickIndices,
  roundedTransitionCorners,
  chartLegendMaxWidth,
  Y_TICK_COUNT,
  X_TICK_MIN_COUNT,
  STEP_CHART_MAX_WIDTH,
  STEP_CHART_HEIGHT,
} from "./chart.ts";
import {
  raceFrames,
  raceMaxValue,
  raceFrameIndex,
  raceTween,
  raceClearProgress,
  pacmanRowProgress,
  pacmanSymbol,
  RACE_VISIBLE_MODELS,
  RACE_TWEEN_STEPS,
  RACE_FINAL_HOLD_TICKS,
  RACE_FINAL_CLEAR_TICKS,
} from "./race.ts";
import { fadeColor, COLOR_LIGHT_YELLOW } from "./colors.ts";
import {
  sortedAgentsByUsage,
  modelTableWidths,
  usageCellText,
  textWidth,
  SHARE_WIDTH,
} from "./table.ts";
import { type UsageRecord, type UsageTotals, totalTokens } from "../types.ts";

function record(
  model: string,
  date: string,
  inTok: number,
  outTok: number,
): UsageRecord {
  return agentRecord("claude", model, date, inTok, outTok);
}
function agentRecord(
  agent: string,
  model: string,
  date: string,
  inTok: number,
  outTok: number,
): UsageRecord {
  return {
    agent,
    model,
    date,
    in_tokens: inTok,
    total_tokens: inTok + outTok,
    out_tokens: outTok,
    cache_read_input_tokens: 0,
    cache_creation_input_tokens: 0,
  };
}
function usage(inTok: number, outTok: number): UsageTotals {
  return {
    in_tokens: inTok,
    total_tokens: 0,
    out_tokens: outTok,
    cache_read_input_tokens: 0,
    cache_creation_input_tokens: 0,
  };
}

describe("chart geometry", () => {
  test("chart area capped for large terminals", () => {
    const area = fixedChartArea({ x: 2, y: 3, width: 120, height: 30 });
    expect(area).toEqual({
      x: 2,
      y: 3,
      width: STEP_CHART_MAX_WIDTH,
      height: STEP_CHART_HEIGHT,
    });
  });

  test("relative chart ranges use full period windows", () => {
    const records = [record("qwen3.7-max", "2026-05-27", 1, 0)];
    expect(chartDateRange("last7", "2026-05-29", records)).toEqual([
      "2026-05-23",
      "2026-05-29",
    ]);
    expect(chartDateRange("last30", "2026-05-29", records)).toEqual([
      "2026-04-30",
      "2026-05-29",
    ]);
  });

  test("all-time chart range uses data extent", () => {
    const records = [
      record("qwen3.7-max", "2026-05-27", 1, 0),
      record("qwen3.7-max", "2026-05-12", 1, 0),
    ];
    expect(chartDateRange("all", "2026-05-29", records)).toEqual([
      "2026-05-12",
      "2026-05-27",
    ]);
  });

  test("chart x boundaries include plot edges", () => {
    expect(chartXBoundary(0, 7, 10, 30)).toBe(10);
    expect(chartXBoundary(7, 7, 10, 30)).toBe(30);
    expect(chartXBoundary(99, 7, 10, 30)).toBe(30);
  });

  test("plot right reserves space for in-chart legend", () => {
    expect(plotRightBeforeLegend(10, 90, 18, 2)).toBe(70);
  });
  test("plot right keeps minimal plot when legend wide", () => {
    expect(plotRightBeforeLegend(10, 20, 18, 2)).toBe(11);
  });

  test("y tick values include zero and max", () => {
    const ticks = yTickValues(90, Y_TICK_COUNT);
    expect(ticks.length).toBe(Y_TICK_COUNT);
    expect(ticks[0]).toBe(0);
    expect(ticks[ticks.length - 1]).toBe(90);
  });

  test("x tick indices draw every day for short ranges", () => {
    expect(xTickIndices(7)).toEqual([0, 1, 2, 3, 4, 5, 6]);
  });
  test("x tick indices draw at least six ticks for long ranges", () => {
    const ticks = xTickIndices(30);
    expect(ticks.length).toBe(X_TICK_MIN_COUNT);
    expect(ticks[0]).toBe(0);
    expect(ticks[ticks.length - 1]).toBe(29);
  });

  test("rounded transitions use directional corners", () => {
    expect(roundedTransitionCorners(8, 3)).toEqual(["╯", "╭"]);
    expect(roundedTransitionCorners(3, 8)).toEqual(["╮", "╰"]);
  });

  test("chart legend width uses longest item", () => {
    expect(chartLegendMaxWidth(["alpha", "beta"])).toBe(7);
  });
});

describe("race", () => {
  test("interpolate empty days between cumulative snapshots", () => {
    const frames = raceFrames([
      record("alpha", "2026-05-27", 100, 20),
      record("beta", "2026-05-29", 200, 0),
    ]);
    expect(frames.length).toBe(3);
    expect(frames[0]!.date).toBe("2026-05-27");
    expect(frames[0]!.entries[0]!.model).toBe("alpha");
    expect(frames[0]!.entries[0]!.value).toBe(120);
    expect(usageCellText(frames[0]!.entries[0]!.usage)).toBe("↑100 ↓20");
    expect(frames[1]!.date).toBe("2026-05-28");
    expect(frames[1]!.entries[0]!.model).toBe("alpha");
    expect(frames[1]!.entries[1]!.model).toBe("beta");
    expect(frames[1]!.entries[1]!.value).toBe(100);
    expect(frames[2]!.date).toBe("2026-05-29");
    expect(frames[2]!.entries[0]!.model).toBe("beta");
    expect(frames[2]!.entries[0]!.value).toBe(200);
    expect(frames[2]!.entries[1]!.model).toBe("alpha");
  });

  test("keeps only top 15 models", () => {
    const records = Array.from({ length: 18 }, (_, idx) =>
      record(
        `model-${idx.toString().padStart(2, "0")}`,
        "2026-05-29",
        idx + 1,
        0,
      ),
    );
    const frames = raceFrames(records);
    const models = frames[0]!.entries.map((e) => e.model);
    expect(models.length).toBe(RACE_VISIBLE_MODELS);
    expect(models[0]).toBe("model-17");
    expect(models[models.length - 1]).toBe("model-03");
  });

  test("max value uses global final scale", () => {
    const frames = raceFrames([
      record("alpha", "2026-05-27", 100, 0),
      record("beta", "2026-05-28", 1000, 0),
    ]);
    expect(frames[0]!.entries[0]!.value).toBe(100);
    expect(raceMaxValue(frames)).toBe(1000);
  });

  test("keeps agent cells for dynamic table", () => {
    const frames = raceFrames([
      agentRecord("claude", "alpha", "2026-05-27", 100, 0),
      agentRecord("codex", "beta", "2026-05-28", 300, 0),
    ]);
    expect(totalTokens(frames[0]!.cells.get("claude\talpha")!)).toBe(100);
    expect(sortedAgentsByUsage(frames[0]!.cells, true)).toEqual([
      ["claude", "Claude Code"],
    ]);
    expect(sortedAgentsByUsage(frames[1]!.cells, true)[0]).toEqual([
      "codex",
      "Codex",
    ]);
  });

  test("frame index advances by tween steps", () => {
    const frameTicks = RACE_TWEEN_STEPS * 3;
    const cycleTicks =
      frameTicks + RACE_FINAL_HOLD_TICKS + RACE_FINAL_CLEAR_TICKS;
    expect(raceFrameIndex(0, 3)).toBe(0);
    expect(raceFrameIndex(RACE_TWEEN_STEPS - 1, 3)).toBe(0);
    expect(raceFrameIndex(RACE_TWEEN_STEPS, 3)).toBe(1);
    expect(raceFrameIndex(frameTicks, 3)).toBe(2);
    expect(raceFrameIndex(cycleTicks - 1, 3)).toBe(2);
    expect(raceFrameIndex(cycleTicks, 3)).toBe(0);
  });

  test("tween reaches final value during hold and clear", () => {
    const frameTicks = RACE_TWEEN_STEPS * 3;
    const cycleTicks =
      frameTicks + RACE_FINAL_HOLD_TICKS + RACE_FINAL_CLEAR_TICKS;
    expect(raceTween(frameTicks, 3)).toBe(1);
    expect(raceTween(cycleTicks - 1, 3)).toBe(1);
  });

  test("clear starts after final hold", () => {
    const frameTicks = RACE_TWEEN_STEPS * 3;
    const clearStart = frameTicks + RACE_FINAL_HOLD_TICKS;
    const cycleTicks = clearStart + RACE_FINAL_CLEAR_TICKS;
    expect(raceClearProgress(frameTicks, 3)).toBe(0);
    expect(raceClearProgress(clearStart - 1, 3)).toBe(0);
    expect(raceClearProgress(clearStart, 3)).toBeGreaterThan(0);
    expect(raceClearProgress(cycleTicks - 1, 3)).toBe(1);
    expect(raceClearProgress(cycleTicks, 3)).toBe(0);
  });

  test("pacman rows clear with small stagger", () => {
    expect(pacmanRowProgress(0, 0, 3)).toBe(0);
    expect(pacmanRowProgress(0.1, 0, 3)).toBeGreaterThan(
      pacmanRowProgress(0.1, 2, 3),
    );
    expect(pacmanRowProgress(1, 2, 3)).toBe(1);
  });

  test("pacman symbol animates open and closed", () => {
    expect(pacmanSymbol(0, 0)).toBe("ᗧ");
    expect(pacmanSymbol(2, 0)).toBe("●");
    expect(pacmanSymbol(4, 0)).toBe("ᗧ");
  });
});

describe("colors", () => {
  test("fade color dims to black", () => {
    expect(fadeColor(COLOR_LIGHT_YELLOW, 0)).toEqual(COLOR_LIGHT_YELLOW);
    expect(fadeColor(COLOR_LIGHT_YELLOW, 1)).toEqual([0, 0, 0]);
  });
});

describe("table", () => {
  test("agent columns sort by usage", () => {
    const cells = new Map<string, UsageTotals>([
      ["claude\tqwen3.7-max", usage(100, 0)],
      ["codex\tgpt-5.4", usage(300, 0)],
      ["copilot\tgpt-5.5", usage(200, 0)],
    ]);
    expect(sortedAgentsByUsage(cells, false)).toEqual([
      ["codex", "Codex"],
      ["copilot", "Copilot"],
      ["claude", "Claude Code"],
      ["zed", "Zed Agent"],
    ]);
  });

  test("agent columns can hide empty agents", () => {
    const cells = new Map<string, UsageTotals>([
      ["claude\tqwen3.7-max", usage(100, 0)],
    ]);
    expect(sortedAgentsByUsage(cells, true)).toEqual([
      ["claude", "Claude Code"],
    ]);
  });

  test("widths keep stat columns compact", () => {
    const sorted: [string, UsageTotals][] = [
      ["qwen3.7-max", usage(174_400_000, 547_900)],
      ["deepseek-v4-pro", usage(45_700_000, 281_400)],
    ];
    const cells = new Map<string, UsageTotals>([
      ["claude\tqwen3.7-max", usage(174_400_000, 547_900)],
      ["claude\tdeepseek-v4-pro", usage(45_700_000, 281_400)],
      ["copilot\tqwen3.7-max", usage(510_900, 59_900)],
    ]);
    const agentColumns = sortedAgentsByUsage(cells, false);
    const widths = modelTableWidths(103, sorted, cells, agentColumns);
    expect(widths[0]!).toBeGreaterThanOrEqual(20);
    expect(widths[1]).toBe(SHARE_WIDTH);
    expect(widths[2]).toBe(textWidth("↑174.4m ↓547.9k"));
    expect(widths[5]).toBe(textWidth("Codex"));
    expect(widths[6]).toBe(textWidth("Zed Agent"));
  });
});
