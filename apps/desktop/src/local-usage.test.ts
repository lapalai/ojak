import { test } from "node:test";
import assert from "node:assert/strict";
import { POLL_BASE_MS, POLL_MAX_MS, boundedStart, costKind, formatPercent, formatUsd, nextPoll, orderedModels, parseLocalUsageReport, readingProgress, tokenParts, usageRange, usageState } from "./local-usage.ts";
import type { LocalUsageReport, UsageTotals } from "./local-usage.ts";

const totals = (overrides: Partial<UsageTotals> = {}): UsageTotals => ({ inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, totalTokens: 0, costUsd: null, unpricedTokens: 0, ...overrides });
const report = (overrides: Partial<LocalUsageReport> = {}): LocalUsageReport => ({
  indexedAt: 1_000, scanning: false, truncated: false, coverageStart: 100, coverageEnd: 900, filesScanned: 3, filesPending: 0, bytesRead: 300, bytesTotal: 300,
  totals: totals(), tools: [], models: [], warnings: [], excludedRecords: 0, duplicateRecords: 0, ...overrides,
});
const rejects = (raw: unknown) => assert.throws(() => parseLocalUsageReport(raw), (error: { code?: string }) => error.code === "LOCAL_USAGE_INVALID");

test("range: today and month start at local midnight / first day, all is unbounded, until is exclusive of the future", () => {
  const now = new Date(2026, 9, 10, 15, 30, 12).getTime();
  assert.equal(usageRange("today", now).sinceMs, new Date(2026, 9, 10).getTime());
  assert.equal(usageRange("month", now).sinceMs, new Date(2026, 9, 1).getTime());
  assert.equal(usageRange("all", now).sinceMs, null);
  assert.equal(usageRange("today", now).untilMs, now + 1);
});

test("parse accepts a native report and keeps counts exact", () => {
  const raw = report({ totals: totals({ inputTokens: 9_007_199_254_740_000, totalTokens: 9_007_199_254_740_000, costUsd: 0 }), warnings: ["CROSS_TOOL_OVERLAP", "CROSS_TOOL_OVERLAP"] });
  const parsed = parseLocalUsageReport(JSON.parse(JSON.stringify(raw)));
  assert.equal(parsed.totals.totalTokens, 9_007_199_254_740_000);
  assert.equal(parsed.totals.costUsd, 0);
  assert.deepEqual(parsed.warnings, ["CROSS_TOOL_OVERLAP"]);
});

test("parse rejects counts that cannot be trusted instead of rounding them", () => {
  rejects(null);
  rejects({ ...report(), totals: totals({ totalTokens: 2 ** 53 }) });
  rejects({ ...report(), totals: totals({ inputTokens: -1 }) });
  rejects({ ...report(), totals: totals({ outputTokens: 1.5 }) });
  rejects({ ...report(), totals: totals({ costUsd: Number.NaN }) });
  rejects({ ...report(), filesPending: "3" });
  rejects({ ...report(), tools: [{ ...totals(), tool: "gemini", sessions: 1 }] });
  rejects({ ...report(), models: [{ ...totals(), tool: "omp", model: "", }] });
});

test("polling continues only while scanning and backs off without progress", () => {
  const scanning = report({ scanning: true, filesScanned: 1, filesPending: 9 });
  assert.equal(nextPoll(null, report(), 0), null);
  assert.equal(nextPoll(scanning, report({ scanning: false, truncated: true, filesPending: 4 }), 0), null);
  assert.deepEqual(nextPoll(null, scanning, 0), { delayMs: POLL_BASE_MS, stalled: 0 });
  const stuck = nextPoll(scanning, scanning, 0)!;
  assert.ok(stuck.delayMs > POLL_BASE_MS && stuck.stalled === 1);
  assert.equal(nextPoll(scanning, scanning, 1000)!.delayMs, POLL_MAX_MS);
  assert.deepEqual(nextPoll(scanning, report({ scanning: true, filesScanned: 2, filesPending: 8 }), 5), { delayMs: POLL_BASE_MS, stalled: 0 });
});

test("reading progress follows bytes read, not tokens or file count, and stays at the start when the total is unknown", () => {
  assert.equal(readingProgress({ bytesRead: 0, bytesTotal: 0 }), 0);
  // One huge file read out of many small ones still counts by size.
  assert.equal(readingProgress({ bytesRead: 250, bytesTotal: 1000 }), 0.25);
  assert.equal(readingProgress({ bytesRead: 1000, bytesTotal: 1000 }), 1);
});

test("empty states distinguish reading, unavailable, nothing found and nothing in this period", () => {
  assert.equal(usageState(report({ totals: totals({ totalTokens: 5 }) })), "data");
  assert.equal(usageState(report({ scanning: true, indexedAt: null, filesScanned: 0 })), "reading");
  assert.equal(usageState(report({ indexedAt: null, filesScanned: 0, warnings: ["INDEX_UNAVAILABLE"] })), "unavailable");
  assert.equal(usageState(report({ indexedAt: null, filesScanned: 0, warnings: ["NO_SOURCES"] })), "none");
  assert.equal(usageState(report({ filesScanned: 0 })), "none");
  assert.equal(usageState(report()), "period-empty");
});

test("unknown pricing is never shown as zero cost", () => {
  assert.equal(costKind(totals()), "none");
  assert.equal(costKind(totals({ totalTokens: 10, unpricedTokens: 10 })), "unpriced");
  assert.equal(costKind(totals({ totalTokens: 10 })), "unpriced");
  assert.equal(costKind(totals({ totalTokens: 10, costUsd: 0 })), "priced");
  assert.equal(costKind(totals({ totalTokens: 10, costUsd: 1.2, unpricedTokens: 3 })), "partial");
  assert.equal(formatUsd(0.001, "en-US"), "<$0.01");
  assert.equal(formatUsd(0, "en-US"), "$0.00");
});

test("token parts sum to the metric components with honest shares", () => {
  const parts = tokenParts(totals({ inputTokens: 1, outputTokens: 1, cacheReadTokens: 98, totalTokens: 100 }));
  assert.deepEqual(parts.map(part => part.value), [1, 1, 98, 0]);
  assert.equal(formatPercent(parts[0].share), "1");
  assert.equal(formatPercent(0.0004), "<1");
  assert.equal(formatPercent(0), "0");
  assert.ok(tokenParts(totals()).every(part => part.share === 0));
});

test("models keep tool order then most tokens, with no rank", () => {
  const model = (tool: string, name: string, tokens: number) => ({ ...totals({ totalTokens: tokens }), tool, model: name });
  const ordered = orderedModels([model("codex", "a", 9), model("omp", "b", 1), model("omp", "c", 5), model("claude", "d", 2)]);
  assert.deepEqual(ordered.map(item => item.model), ["c", "b", "d", "a"]);
});

test("a bounded start is reported only when it cuts into the requested period", () => {
  const range = { sinceMs: 100, untilMs: 1000 };
  assert.equal(boundedStart(report({ truncated: true, coverageStart: 500 }), range), 500);
  assert.equal(boundedStart(report({ truncated: true, coverageStart: 50 }), range), null);
  assert.equal(boundedStart(report({ truncated: false, coverageStart: 500 }), range), null);
  assert.equal(boundedStart(report({ truncated: true, coverageStart: 500 }), { sinceMs: null, untilMs: 1000 }), 500);
});
