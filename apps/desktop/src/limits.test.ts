import { test } from "node:test";
import assert from "node:assert/strict";
import { allResting, bucketState, limitsOf, remainingTone, tightestOf } from "./limits.ts";
import type { QuotaBucket } from "./types.ts";

const bucket = (id: string, label: string, used: number, model: string | null = null): QuotaBucket =>
  ({ id, label, model, usedPercent: used, resetsAt: null, observedAt: 1, source: "test", status: "known" });
const known = () => "known";

test("remaining color depends only on remaining percent, with reserve taking precedence over warning", () => {
  assert.deepEqual([100, 31, 30, 11, 10, 0, null].map(value => remainingTone(value, 10)), ["ok", "ok", "warn", "warn", "bad", "bad", "unknown"]);
  // 여유분이 경고 기준보다 커도 여유분 이하는 위험으로 본다.
  assert.equal(remainingTone(35, 40), "bad");
});

test("the out-of-quota strip shows only when every included account is resting", () => {
  const r = (until: number | null, off = false) => ({ kind: "resting", until, off });
  assert.deepEqual(allResting([r(300), r(200)]), { until: 200 });
  // 하나라도 쓸 수 있거나 판정이 불확실하면 띄우지 않는다.
  assert.equal(allResting([r(300), { kind: "available", until: null, off: false }]), false);
  assert.equal(allResting([r(300), { kind: "unknown", until: null, off: false }]), false);
  // 배정에서 뺀 계정은 세지 않지만, 남은 계정이 없으면 띄우지 않는다.
  assert.deepEqual(allResting([r(300), { kind: "available", until: null, off: true }]), { until: 300 });
  assert.equal(allResting([r(300, true)]), false);
  assert.deepEqual(allResting([r(null)]), { until: null });
});

test("same suffix for different model limits stays separate and the used model decides the card value", () => {
  // Antigravity: Gemini 주간은 거의 남지 않았지만, 지금은 Claude를 쓴다.
  const buckets = [bucket("g", "Gemini · 주간", 95, "gemini"), bucket("c", "Claude & GPT · 주간", 40, "claude")];
  const limits = limitsOf(buckets, ["claude-opus-5-5"], known);
  assert.deepEqual(limits.map(limit => [limit.label, limit.remaining, limit.relevant]), [["Gemini 주간", 5, false], ["Claude & GPT 주간", 60, true]]);
  assert.equal(tightestOf(limits), 60);
});

test("the same limit seen by two tools is shown once with the lower remaining value", () => {
  const buckets = [bucket("a", "Claude · 주간", 30), bucket("b", "OMP · 주간", 30.2), bucket("c", "Claude · 5시간", 10)];
  const limits = limitsOf(buckets, [], known);
  assert.deepEqual(limits.map(limit => limit.label), ["주간", "5시간"]);
  assert.equal(tightestOf(limits), 69.8);
});

test("a model-only limit counts only while that model is in use", () => {
  const buckets = [bucket("w", "주간", 20), bucket("f", "Fable 주간", 98, "fable")];
  assert.equal(tightestOf(limitsOf(buckets, ["claude-opus-5-5"], known)), 80);
  assert.equal(tightestOf(limitsOf(buckets, ["claude-fable-5-1"], known)), 2);
});

test("exhausted counts as zero and unobserved limits do not set the value", () => {
  const buckets = [bucket("x", "5시간", 100), bucket("y", "주간", 10)];
  const limits = limitsOf(buckets, [], candidate => candidate.id === "x" ? "exhausted" : "unknown");
  assert.deepEqual(limits.map(limit => limit.remaining), [0, null]);
  assert.equal(tightestOf(limits), 0);
});

test("an exhausted bucket whose reset passed or whose observation is old is stale, not 0%", () => {
  const now = 10_000_000;
  const exhausted = (observedAt: number, resetsAt: number | null): QuotaBucket => ({ ...bucket("e", "주간", 100), status: "exhausted", observedAt, resetsAt });
  assert.equal(bucketState(exhausted(now - 60_000, now + 60_000), 900, now), "exhausted");
  assert.equal(bucketState(exhausted(now - 60_000, now - 1), 900, now), "stale");
  assert.equal(bucketState(exhausted(now - 901_000, now + 60_000), 900, now), "stale");
  assert.equal(tightestOf(limitsOf([exhausted(now - 60_000, now - 1)], [], candidate => bucketState(candidate, 900, now))), null);
});

