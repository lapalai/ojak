import { test } from "node:test";
import assert from "node:assert/strict";
import { busyItems, needsConsent, restartGuidance } from "./serviceRestart.ts";
import type { BusyReport } from "./serviceRestart.ts";

const idle: BusyReport = { tools: [], omp: { known: true, streams: 0, queued: 0 } };

test("nothing busy restarts without a prompt", () => {
  assert.deepEqual(busyItems(idle), []);
  assert.equal(needsConsent(idle), false);
  assert.deepEqual(restartGuidance(idle), { omp: false, terminals: false });
});

test("omp streams alone, queued omp requests alone, and terminal sessions alone each need consent", () => {
  assert.equal(needsConsent({ ...idle, omp: { known: true, streams: 2, queued: 0 } }), true);
  assert.equal(needsConsent({ ...idle, omp: { known: true, streams: 0, queued: 1 } }), true);
  assert.equal(needsConsent({ ...idle, tools: [{ tool: "claude", active: 0, uncertain: 1 }] }), true);
  // 개수 0인 도구 줄은 확인을 요구하지 않는다.
  assert.equal(needsConsent({ ...idle, tools: [{ tool: "codex", active: 0, uncertain: 0 }] }), false);
});

test("an unreadable omp state is shown as unknown and needs consent, never as zero", () => {
  const unknown: BusyReport = { tools: [], omp: { known: false, streams: 0, queued: 0 } };
  assert.deepEqual(busyItems(unknown), [{ kind: "ompUnknown" }]);
  assert.equal(needsConsent(unknown), true);
  assert.deepEqual(restartGuidance(unknown), { omp: true, terminals: false });
});

test("the confirm list adds active and uncertain terminal sessions per tool and keeps omp streams separate from queued requests", () => {
  const busy: BusyReport = {
    tools: [{ tool: "claude", active: 2, uncertain: 1 }, { tool: "codex", active: 0, uncertain: 2 }],
    omp: { known: true, streams: 3, queued: 1 },
  };
  assert.deepEqual(busyItems(busy), [
    { kind: "ompStreams", count: 3 },
    { kind: "ompQueued", count: 1 },
    { kind: "terminal", tool: "claude", count: 3 },
    { kind: "terminal", tool: "codex", count: 2 },
  ]);
});

test("guidance covers only what was actually cut or kept", () => {
  assert.deepEqual(restartGuidance({ ...idle, omp: { known: true, streams: 1, queued: 0 } }), { omp: true, terminals: false });
  assert.deepEqual(restartGuidance({ ...idle, tools: [{ tool: "claude", active: 1, uncertain: 0 }] }), { omp: false, terminals: true });
  assert.deepEqual(
    restartGuidance({ tools: [{ tool: "codex", active: 1, uncertain: 0 }], omp: { known: true, streams: 0, queued: 2 } }),
    { omp: true, terminals: true },
  );
});
