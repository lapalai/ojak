import { test } from "node:test";
import assert from "node:assert/strict";
import { compareRows, criticalLimit, groupAlias, canRelogin, choiceLabel, loginTarget, meaningfulLabel, overviewOf, quotaRows, quotaTone, watchable, watchedMembers, whenBand, windowKind } from "./overview.ts";
import { quotaReading } from "./limits.ts";
import type { Account, AccountQuotaSummary, QuotaBucket } from "./types.ts";

const NOW = 1_000_000_000_000;
const HOUR = 3_600_000;
const STALE = 900;
const bucket = (id: string, label: string, used: number, resetsIn: number | null, model: string | null = null, observedAgo = 1000, status = "known"): QuotaBucket =>
  ({ id, label, model, usedPercent: used, resetsAt: resetsIn === null ? null : NOW + resetsIn, observedAt: NOW - observedAgo, source: "test", status });
const quotas = (...buckets: QuotaBucket[]) => quotaRows(buckets, STALE, NOW);
const verdict = (overrides: Partial<AccountQuotaSummary>): AccountQuotaSummary =>
  ({ accountIds: ["a"], kind: "available", until: null, models: [], label: null, rate: false, ...overrides });
const account = (overrides: Partial<Account>): Account => ({ id: "a", provider: "anthropic", tool: "claude", label: "", email: null, organization: null, plan: null, profilePath: null, binaryPath: null, identityKey: null,
  authStatus: "authenticated", verification: "verified", canLaunch: true, reason: null, enabled: true, maxConcurrency: 1, buckets: [], lastCheckedAt: 0, ...overrides });

test("available with 14% left warns, 31% left stays plain available", () => {
  const low = overviewOf(verdict({}), quotas(bucket("w", "Claude 7 Day · 주간", 86, 3 * HOUR)), NOW);
  assert.equal(low.state, "availableLow");
  assert.equal(low.tone, "warning");
  assert.equal(low.limit?.left, 14);
  assert.equal(overviewOf(verdict({}), quotas(bucket("w", "주간", 69, 3 * HOUR)), NOW).state, "available");
  assert.equal(overviewOf(verdict({}), quotas(bucket("w", "주간", 70, 3 * HOUR)), NOW).state, "availableLow");
});

test("the critical limit is the tightest fresh common limit; model-only limits are not flattened into it", () => {
  const rows = quotas(bucket("5h", "Claude 5 Hour · 5시간", 20, HOUR), bucket("7d", "Claude 7 Day · 주간", 55, 30 * HOUR), bucket("f", "Fable 주간", 97, 30 * HOUR, "fable"));
  const limit = criticalLimit(rows);
  assert.equal(limit?.label, "주간");
  assert.equal(limit?.left, 45);
  // 모델 전용 한도는 따로 이름을 붙여 말한다.
  const view = overviewOf(verdict({}), rows, NOW);
  assert.equal(view.limit?.left, 45);
  assert.equal(view.modelLimit?.model, "fable");
  assert.equal(view.modelLimit?.left, 3);
});

test("a model-only reading is the only one shown when no common limit is fresh, and it stays model-named", () => {
  const view = overviewOf(verdict({}), quotas(bucket("f", "Fable 주간", 40, 30 * HOUR, "fable")), NOW);
  assert.equal(view.limit, null);
  assert.equal(view.modelLimit?.model, "fable");
});

test("stale readings never become the collapsed critical limit", () => {
  const rows = quotas(bucket("w", "주간", 50, 3 * HOUR, null, STALE * 1000 + 5000));
  assert.equal(criticalLimit(rows), null);
  const view = overviewOf(verdict({ kind: "unknown" }), rows, NOW);
  assert.equal(view.limit, null);
  assert.equal(view.state, "unknown");
  assert.equal(view.when, null);
  // 자세히 보기에서만 마지막 관측으로 남는다.
  assert.equal(quotaReading(rows[0].bucket, rows[0].state).confirmed, false);
});

test("resting blames the weekly limit even when the 5-hour limit resets later, and picks the limit that frees last", () => {
  const rows = quotas(bucket("5h", "Claude 5 Hour · 5시간", 100, HOUR, null, 1000, "exhausted"), bucket("7d", "Claude 7 Day · 주간", 100, 50 * HOUR, null, 1000, "exhausted"));
  const view = overviewOf(verdict({ kind: "resting", until: NOW + 50 * HOUR, label: "Claude 7 Day · 주간" }), rows, NOW);
  assert.deepEqual(view.blocker, { kind: "weekly" });
  assert.equal(view.blockedBy, "quota");
  assert.deepEqual(view.when, { kind: "recovery", at: NOW + 50 * HOUR });
  const fiveHourOnly = overviewOf(verdict({ kind: "resting", until: NOW + HOUR, label: "Claude 5 Hour · 5시간" }), quotas(bucket("5h", "5시간", 100, HOUR, null, 1000, "exhausted")), NOW);
  assert.deepEqual(fiveHourOnly.blocker, { kind: "fiveHour" });
});

test("resting by request rate or by an unlabeled block does not invent a window", () => {
  assert.deepEqual(overviewOf(verdict({ kind: "resting", rate: true, until: NOW + HOUR }), [], NOW).blocker, { kind: "rate" });
  assert.deepEqual(overviewOf(verdict({ kind: "resting" }), [], NOW).blocker, { kind: "generic" });
  const unknownWhen = overviewOf(verdict({ kind: "resting" }), [], NOW);
  assert.deepEqual(unknownWhen.when, { kind: "unknown", at: null });
  assert.deepEqual(overviewOf(verdict({ kind: "resting", label: "Gemini · 일일" }), [], NOW).blocker, { kind: "limit", label: "일일" });
});

test("blocked accounts mute leftover quota instead of coloring it green", () => {
  const leftover = quotaReading(quotas(bucket("5h", "5시간", 0, HOUR))[0].bucket, "known");
  assert.equal(quotaTone(leftover, "quota", 10), "muted");
  assert.equal(quotaTone(leftover, "account", 10), "muted");
  assert.equal(quotaTone(leftover, null, 10), "ok");
  const spent = quotaReading(quotas(bucket("7d", "주간", 100, HOUR, null, 1000, "exhausted"))[0].bucket, "exhausted");
  assert.equal(quotaTone(spent, "quota", 10), "bad");
  const stale = quotaReading(quotas(bucket("7d", "주간", 20, HOUR, null, STALE * 1000 + 1))[0].bucket, "stale");
  assert.equal(quotaTone(stale, null, 10), "unknown");
});

test("partial names the limited models and uses only a model reset that is still in the future", () => {
  const rows = quotas(bucket("7d", "주간", 20, 5 * HOUR), bucket("f", "Fable 주간", 100, 3 * HOUR, "fable", 1000, "exhausted"));
  const view = overviewOf(verdict({ kind: "partial", models: ["Fable"] }), rows, NOW);
  assert.deepEqual(view.models, ["Fable"]);
  assert.equal(view.blockedBy, null);
  assert.deepEqual(view.when, { kind: "modelBack", at: NOW + 3 * HOUR });
  assert.deepEqual(overviewOf(verdict({ kind: "partial", models: ["Fable"] }), [], NOW).when, { kind: "unknown", at: null });
});

test("reserve keeps the reset time and does not claim a blocker", () => {
  const view = overviewOf(verdict({ kind: "reserve", label: "주간" }), quotas(bucket("w", "주간", 95, 8 * HOUR)), NOW);
  assert.equal(view.state, "reserve");
  assert.equal(view.blocker, null);
  assert.deepEqual(view.when, { kind: "reset", at: NOW + 8 * HOUR });
});

test("an available account with no provided reset time says so instead of staying silent", () => {
  assert.deepEqual(overviewOf(verdict({}), quotas(bucket("w", "주간", 10, null)), NOW).when, { kind: "unknown", at: null });
  // 한도 정보가 아예 없으면 시각 칸도 비운다.
  assert.equal(overviewOf(verdict({}), [], NOW).when, null);
});

test("an expiring weekly limit takes the next-time slot", () => {
  const view = overviewOf(verdict({ expiring: { label: "주간", resetsAt: NOW + 5 * HOUR, usablePercent: 60, perHour: 12 } }), quotas(bucket("w", "주간", 30, 5 * HOUR)), NOW);
  assert.deepEqual(view.when, { kind: "expiring", at: NOW + 5 * HOUR, percent: 60 });
});

test("login and excluded rows carry no quota claim", () => {
  for (const kind of ["login", "excluded"] as const) {
    const view = overviewOf(verdict({ kind }), quotas(bucket("w", "주간", 5, HOUR)), NOW);
    assert.equal(view.blockedBy, "account");
    assert.equal(view.limit, null);
  }
});

test("credit and extra-usage rows are quota-blocked and still say when the subscription limit frees", () => {
  const view = overviewOf(verdict({ kind: "credits", until: NOW + 4 * HOUR, credits: { active: true, unlimited: false, balance: "3" } }), quotas(bucket("w", "주간", 100, 4 * HOUR, null, 1000, "exhausted")), NOW);
  assert.equal(view.blockedBy, "quota");
  assert.deepEqual(view.when, { kind: "reset", at: NOW + 4 * HOUR });
  assert.equal(view.tone, "warning");
});

test("windows are recognised from provider labels in any language", () => {
  assert.deepEqual(["Claude 7 Day · 주간", "Claude 5 Hour · 5시간", "Weekly", "5h", "Gemini · 일일"].map(windowKind), ["weekly", "fiveHour", "weekly", "fiveHour", "other"]);
});

test("rows are ordered by name, never by usage, and unknown-route rows come last", () => {
  const row = (key: string, sortKey: string, unknown = false, hasGroup = true) => ({ key, sortKey, unknown, group: hasGroup ? ({} as never) : null });
  const sorted = [row("z", "zed"), row("route", "", true, false), row("a", "alpha"), row("reg", "beta", false, false)].sort(compareRows).map(item => item.key);
  assert.deepEqual(sorted, ["a", "z", "reg", "route"]);
});


test("only a user-chosen label counts as an account name", () => {
  assert.equal(meaningfulLabel("Claude Code · 기본 프로필"), null);
  assert.equal(meaningfulLabel("Claude Code · 기본 프로필 · a@b.com"), null);
  assert.equal(meaningfulLabel("Codex · default profile · a@b.com"), null);
  assert.equal(meaningfulLabel("OMP · anthropic · a@b.com"), null);
  assert.equal(meaningfulLabel("a@b.com"), null);
  assert.equal(meaningfulLabel("Work Claude"), "Work Claude");
  assert.equal(meaningfulLabel("Claude Code · Work · a@b.com"), "Claude Code · Work");
  assert.equal(groupAlias([account({ label: "Claude Code · 기본 프로필" }), account({ id: "b", tool: "omp", label: "OMP · anthropic" })]), null);
  assert.equal(groupAlias([account({ label: "Claude Code · 기본 프로필" }), account({ id: "b", label: "Personal" })]), "Personal");
});

test("short times switch from relative to weekday to date, and a past time is pending, never a date", () => {
  assert.equal(whenBand(NOW - 1, NOW), "pending");
  assert.equal(whenBand(NOW + 23 * HOUR, NOW), "relative");
  assert.equal(whenBand(NOW + 25 * HOUR, NOW), "weekday");
  assert.equal(whenBand(NOW + 7 * 24 * HOUR, NOW), "date");
});

test("recovery watch is offered only for limited states and reports any watched group member", () => {
  assert.deepEqual(["available", "availableLow", "reserve", "login", "excluded", "unknown"].map(state => watchable(state as never)), [false, false, false, false, false, false]);
  assert.deepEqual(["resting", "partial", "credits", "extra"].map(state => watchable(state as never)), [true, true, true, true]);
  assert.deepEqual(watchedMembers(["b", "x"], ["a", "b"]), ["b"]);
  assert.deepEqual(watchedMembers(null, ["a"]), []);
});

test("re-login targets only an official CLI member that asks for sign-in and is installed", () => {
  const needs = account({ id: "c", canLaunch: false, authStatus: "auth-required", tool: "codex" });
  const omp = account({ id: "o", canLaunch: false, authStatus: "auth-required", tool: "omp" });
  assert.equal(loginTarget([omp, needs], () => true)?.id, "c");
  assert.equal(loginTarget([omp, needs], () => false), null);
  assert.equal(loginTarget([omp], () => true), null);
});

test("re-login also targets a Claude or Codex profile in error state, but never omp or a profile-less account", () => {
  const drifted = account({ id: "d", canLaunch: false, authStatus: "error", tool: "claude", profilePath: "/p/.claude" });
  const noProfile = account({ id: "n", canLaunch: false, authStatus: "error", tool: "claude", profilePath: null });
  const ompError = account({ id: "o", canLaunch: false, authStatus: "error", tool: "omp", profilePath: "/p/omp" });
  const launchable = account({ id: "l", canLaunch: true, authStatus: "error", tool: "codex", profilePath: "/p/.codex" });
  assert.equal(loginTarget([ompError, noProfile, launchable, drifted], () => true)?.id, "d");
  assert.equal(loginTarget([drifted], () => false), null);
  assert.equal(loginTarget([ompError, noProfile, launchable], () => true), null);
  assert.equal(canRelogin(drifted), true);
  assert.equal(canRelogin(ompError), false);
});

test("same-tool accounts sharing a label stay distinguishable, by email when visible and by position when masked or emailless", () => {
  const a = account({ id: "a", tool: "claude", label: "Work", email: "a@x.com" });
  const b = account({ id: "b", tool: "claude", label: "Work", email: "b@x.com" });
  const c = account({ id: "c", tool: "codex", label: "Work", email: "c@x.com" });
  const d = account({ id: "d", tool: "claude", label: "Home", email: "d@x.com" });
  const e = account({ id: "e", tool: "claude", label: "Work", email: null });
  assert.equal(choiceLabel(a, [a, b, c, d], false), "Work · a@x.com");
  assert.equal(choiceLabel(b, [a, b, c, d], false), "Work · b@x.com");
  assert.equal(choiceLabel(c, [a, b, c, d], false), "Work");
  assert.equal(choiceLabel(d, [a, b, c, d], false), "Home");
  assert.equal(choiceLabel(a, [a, b], true), "Work #1");
  assert.equal(choiceLabel(b, [a, b], true), "Work #2");
  assert.equal(choiceLabel(e, [a, e], false), "Work #2");
});
