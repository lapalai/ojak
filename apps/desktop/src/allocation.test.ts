import { test } from "node:test";
import assert from "node:assert/strict";
import { allocatableTools, anyAllocatable, concurrencyRows, concurrencyTargets, concurrencyUpdateIds, concurrencyValue, parseAllocation } from "./allocation.ts";

const link = (over: Partial<{ id: string; provider: string; tool: string; canLaunch: boolean; verification: string; identityKey: string | null; maxConcurrency: number; ompCredentialPins: { provider: string; hash: string }[] }> = {}) =>
  ({ id: "a", provider: "anthropic", tool: "claude", canLaunch: true, verification: "preflight-verified", identityKey: "anthropic|subject:s1|workspace:w1", maxConcurrency: 1, ...over });

test("a setup result without allocation data is never read as allocatable", () => {
  assert.equal(parseAllocation(undefined).state, "unavailable");
  assert.equal(parseAllocation({ state: "ready" }).state, "unavailable");
  assert.equal(anyAllocatable([{ allocation: parseAllocation(null) }]), false);
});

test("allocation codes keep only service reason codes and the real state", () => {
  const parsed = parseAllocation({ state: "excluded", candidates: 2, eligible: 0, codes: ["QUOTA_UNKNOWN", "person@example.test", 7, "AUTH_REQUIRED"], errorCode: "NO_ELIGIBLE_ACCOUNT" });
  assert.deepEqual(parsed, { state: "excluded", candidates: 2, eligible: 0, codes: ["QUOTA_UNKNOWN", "AUTH_REQUIRED"], errorCode: "NO_ELIGIBLE_ACCOUNT" });
  assert.equal(anyAllocatable([{ allocation: parsed }, { allocation: parseAllocation({ state: "allocatable", eligible: 1 }) }]), true);
});

test("only verified launchable Claude links can change parallelism, and observed rows are left out", () => {
  const rows = [link({ id: "claude" }), link({ id: "codex", tool: "codex" }), link({ id: "omp", tool: "omp", canLaunch: false }), link({ id: "unverified", verification: "observed" }), link({ id: "blocked", canLaunch: false })];
  assert.deepEqual(concurrencyTargets(rows).map(row => row.id), ["claude"]);
});

test("existing values are preserved until the user picks one, and equal links are not rewritten", () => {
  const targets = [link({ id: "a", maxConcurrency: 4 }), link({ id: "b", maxConcurrency: 4 })];
  assert.equal(concurrencyValue(targets), 4);
  assert.deepEqual(concurrencyUpdateIds(targets, 4), []);
  assert.deepEqual(concurrencyUpdateIds(targets, 0), ["a", "b"]);
  const mixed = [link({ id: "a", maxConcurrency: 1 }), link({ id: "b", maxConcurrency: 6 })];
  assert.equal(concurrencyValue(mixed), "mixed");
  assert.deepEqual(concurrencyUpdateIds(mixed, 6), ["a"]);
  assert.equal(concurrencyValue([]), null);
});

test("the terminal tool comes from the service decision, not from canLaunch", () => {
  const tools = [
    { tool: "claude", allocation: parseAllocation({ state: "excluded", codes: ["QUOTA_EXHAUSTED"] }) },
    { tool: "codex", allocation: parseAllocation({ state: "allocatable", eligible: 1 }) },
    { tool: "legacy" },
  ];
  assert.deepEqual(allocatableTools(tools), ["codex"]);
});

test("one parallelism row is one exact execution identity, never a display group", () => {
  // 같은 워크스페이스의 다른 사용자, 같은 사용자의 다른 워크스페이스는 화면 묶음으로는 합쳐질 수 있지만 서로 다른 실행 identity다.
  const rows = concurrencyRows([
    link({ id: "a1", identityKey: "anthropic|subject:s1|workspace:w1" }),
    link({ id: "a2", identityKey: "anthropic|subject:s1|workspace:w1", maxConcurrency: 3 }),
    link({ id: "other-user", identityKey: "anthropic|subject:s2|workspace:w1" }),
    link({ id: "other-workspace", identityKey: "anthropic|subject:s1|workspace:w2" }),
    link({ id: "codex", tool: "codex", identityKey: "anthropic|subject:s1|workspace:w1" }),
    link({ id: "omp", tool: "omp", canLaunch: false, identityKey: "anthropic|subject:s1|workspace:w1" }),
  ]);
  assert.deepEqual(rows.map(row => row.map(item => item.id)), [["a1", "a2"], ["other-user"], ["other-workspace"]]);
  // 한 행만 바꾸면 그 행의 연결만 쓴다.
  assert.deepEqual(concurrencyUpdateIds(rows[0], 3), ["a1"]);
  assert.deepEqual(concurrencyUpdateIds(rows[1], 3), ["other-user"]);
});

test("links sharing a credential pin form one row and an unrelated identity key stays apart", () => {
  const pin = { provider: "anthropic", hash: "h" };
  const rows = concurrencyRows([
    link({ id: "x", identityKey: "k1", ompCredentialPins: [pin] }),
    link({ id: "y", identityKey: "k2" }),
    link({ id: "z", identityKey: "k3", ompCredentialPins: [pin] }),
    link({ id: "other-provider", provider: "openai", identityKey: "k2" }),
  ]);
  assert.deepEqual(rows.map(row => row.map(item => item.id).sort()), [["x", "z"], ["y"], ["other-provider"]]);
});
