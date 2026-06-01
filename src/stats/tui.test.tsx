import React from "react";
import { test, expect, describe } from "bun:test";
import { render } from "ink-testing-library";

import { StatsApp } from "./tui.tsx";
import type { UsageRecord } from "./types.ts";

function rec(
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

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const records: UsageRecord[] = [
  rec("claude", "qwen3.7-max", "2026-05-27", 174_400_000, 547_900),
  rec("codex", "gpt-5.4", "2026-05-28", 45_700_000, 281_400),
  rec("copilot", "glm-5", "2026-05-29", 510_900, 59_900),
];

describe("StatsApp (ink)", () => {
  test("renders overview header, chart and model table", async () => {
    const { lastFrame, unmount } = render(
      <StatsApp records={records} today="2026-05-29" onExit={() => {}} />,
    );
    await sleep(30);
    const frame = lastFrame() ?? "";
    expect(frame).toContain("cx stats");
    expect(frame).toContain("Overview");
    expect(frame).toContain("Tokens per Day");
    expect(frame).toContain("Models");
    expect(frame).toContain("qwen3.7-max");
    unmount();
  });

  test("Tab switches to dynamicview race", async () => {
    const { lastFrame, stdin, unmount } = render(
      <StatsApp records={records} today="2026-05-29" onExit={() => {}} />,
    );
    await sleep(30);
    stdin.write("\t");
    await sleep(120);
    const frame = lastFrame() ?? "";
    expect(frame).toContain("Model Tokens Top 15");
    expect(frame).toContain("Dynamic Models");
    unmount();
  });

  test("q triggers exit", async () => {
    let exited = false;
    const { stdin, unmount } = render(
      <StatsApp
        records={records}
        today="2026-05-29"
        onExit={() => {
          exited = true;
        }}
      />,
    );
    await sleep(30);
    stdin.write("q");
    await sleep(30);
    expect(exited).toBe(true);
    unmount();
  });
});
