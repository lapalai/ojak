import { test } from "node:test";
import assert from "node:assert/strict";
import { observedRouteModels } from "./usage-routes.ts";
import type { ObservedAttribution } from "./types.ts";

const now = 1_000_000;
const canonical = (provider: string) => provider === "ojak-claude" ? "anthropic" : provider;
function call(overrides: Partial<ObservedAttribution> = {}): ObservedAttribution {
  return { sessionId: "s1", role: "auxiliary", provider: "anthropic", model: "m", accountId: null,
    verification: "unverified", recordedAt: now, stopReason: null, source: "extension", reason: null, ...overrides };
}

test("completion and legacy selection records do not assert a request route", () => {
  const result = observedRouteModels([{ cwd: null, attributions: [
    call(), call({ route: null }), call({ provider: "ojak-claude", model: "legacy" }),
  ] }], 0, now, canonical);
  assert.deepEqual(result, []);
});

test("bridge requests with upstream completion records do not create unknown warnings", () => {
  const result = observedRouteModels([{ cwd: "/example", attributions: [
    call({ provider: "ojak-claude", route: "bridge" }), call({ route: null }),
  ] }], 0, now, canonical);
  assert.deepEqual(result, []);
});

test("bridge evidence cannot hide explicit direct or unknown requests in the same session", () => {
  const result = observedRouteModels([{ cwd: "/same", attributions: [
    call({ provider: "ojak-claude", route: "bridge" }), call({ route: "direct" }),
    call({ route: "unknown" }), call(),
  ] }], 0, now, canonical);
  assert.deepEqual(result.map(row => [row.model, row.route]), [["m", "direct"], ["m", "unknown"]]);
});

test("counts models, not duplicate sources, roles or projects", () => {
  const result = observedRouteModels([
    { cwd: "/one", attributions: [call({ route: "direct", role: "main" }), call({ route: "direct", source: "session-file" })] },
    { cwd: "/two", attributions: [call({ route: "direct", sessionId: "s2" })] },
  ], 0, now, canonical);
  assert.deepEqual(result.map(row => ({ model: row.model, projects: row.projects, roles: row.roles })), [
    { model: "m", projects: ["/one", "/two"], roles: ["main", "auxiliary"] },
  ]);
});

test("persisted route evidence follows selected time range, not one-hour bridge memory", () => {
  const old = call({ route: "direct", recordedAt: now - 120_000, source: "session-file" });
  const snapshot = [{ cwd: "/old", attributions: [old, call({ verification: "configured", model: "setting" }), call({ recordedAt: now + 1, model: "future" })] }];
  assert.deepEqual(observedRouteModels(snapshot, now - 60_000, now, canonical), []);
  assert.deepEqual(observedRouteModels(snapshot, now - 120_000, now, canonical).map(row => [row.model, row.route]), [["m", "direct"]]);
});

test("project timestamps stay separate across routes, paths and duplicate observations", () => {
  const result = observedRouteModels([
    { cwd: "/work/a/shop", attributions: [call({ route: "direct", recordedAt: now - 10 }), call({ route: "direct", recordedAt: now - 20 }), call({ route: "unknown", recordedAt: now - 5 })] },
    { cwd: "/work/b/shop", attributions: [call({ route: "direct", recordedAt: now - 30 })] },
    { cwd: null, attributions: [call({ route: "direct", recordedAt: now - 40 }), call({ route: "bridge" })] },
  ], now - 60, now, canonical);
  assert.deepEqual(result.find(row => row.route === "direct")?.observations, [
    { project: "/work/a/shop", lastRecordedAt: now - 10 },
    { project: "/work/b/shop", lastRecordedAt: now - 30 },
    { project: null, lastRecordedAt: now - 40 },
  ]);
  assert.deepEqual(result.find(row => row.route === "unknown")?.observations, [
    { project: "/work/a/shop", lastRecordedAt: now - 5 },
  ]);
});
