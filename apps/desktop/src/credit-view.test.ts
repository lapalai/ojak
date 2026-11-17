import { test } from "node:test";
import assert from "node:assert/strict";
import { balanceStatus, keyCreditView, paidRoutes, providerCredit, providerCreditText } from "./credit-view.ts";
import { dictionaries, locale } from "./i18n.ts";
import type { Account, ApiCreditGrantPolicy, ApiCreditGrantStatus, Policy, ProactiveBlock } from "./types.ts";

const NOW = Date.UTC(2026, 10, 17, 12);
const DAY = 86_400_000;
const key = (id: string, provider: string, enabled = true) => ({ id, tool: "api", provider, enabled } as Account);
const grant = (id: string, accountIds: string[], over: Partial<ApiCreditGrantPolicy> = {}): ApiCreditGrantPolicy =>
  ({ id, accountIds, budgetUsd: 200, startsAt: NOW - 10 * DAY, expiresAt: NOW + 10 * DAY, confirmedAt: NOW - 10 * DAY, source: "manual", ...over });
const status = (id: string, accountIds: string[], state: ApiCreditGrantStatus["state"], remainingUsd: number, blockedBy: ProactiveBlock | null = null, over: Partial<ApiCreditGrantStatus> = {}): ApiCreditGrantStatus =>
  ({ id, accountIds, source: "manual", state, budgetUsd: 200, usedUsd: 200 - remainingUsd, remainingUsd, startsAt: NOW - 10 * DAY, expiresAt: NOW + 10 * DAY, unmeteredRequests: 0, unmetered: false, perDayUsd: null, monthlyLimitReached: false, proactive: { allowed: blockedBy === null, blockedBy }, ...over });

test("only the OpenAI API switch on still counts as paid fallback allowed", () => {
  assert.deepEqual(paidRoutes({ useOpenaiApiAfterLimit: true }), ["openaiApi"]);
  assert.deepEqual(paidRoutes({}), []);
  assert.deepEqual(paidRoutes({ useCreditsAfterLimit: true, useExtraUsageAfterLimit: true, useApiCreditAfterLimit: true, useOpenaiApiAfterLimit: true }), ["codexCredits", "claudeExtra", "claudeApi", "openaiApi"]);
});

test("a pool shared by several keys is counted once, expired leftovers are reported as lost, other providers are ignored", () => {
  const accounts = [key("a1", "anthropic"), key("a2", "anthropic"), key("a3", "anthropic"), key("a4", "anthropic", false), key("o1", "openai")];
  const policy = { apiCreditGrants: [
    grant("shared", ["a1", "a2"], { expiresAt: NOW + 3 * DAY }),
    grant("old", ["a3"], { expiresAt: NOW - DAY }),
    grant("oa", ["o1"], { expiresAt: NOW + DAY }),
  ] } as Policy;
  const statuses = [status("shared", ["a1", "a2"], "active", 150), status("old", ["a3"], "expired", 179.45), status("oa", ["o1"], "active", 60)];
  const claude = providerCredit({ accounts, policy, apiCreditGrants: statuses }, "anthropic", NOW);
  assert.equal(claude.leftUsd, 150);
  assert.equal(claude.nextExpiry, NOW + 3 * DAY);
  assert.equal(claude.lostUsd, 179.45);
  // 꺼진 키(a4)는 쓰이지 않으므로 미등록 위험에 넣지 않는다.
  assert.equal(claude.unsetKeys, 0);
  assert.equal(claude.keys, 4);
  const openai = providerCredit({ accounts, policy, apiCreditGrants: statuses }, "openai", NOW);
  assert.deepEqual([openai.leftUsd, openai.nextExpiry, openai.lostUsd], [60, NOW + DAY, 0]);
});

const text = (key: keyof typeof dictionaries.en, params: Record<string, string | number> = {}) => {
  const message = dictionaries[locale][key];
  const raw = typeof message === "string" ? message : Number(params.count) === 1 ? message.one : message.other;
  return raw.replace(/\{(\w+)\}/g, (_, name: string) => String(params[name]));
};

test("without service status the summary says the balance is unknown, never 'no credit' or $0", () => {
  const accounts = [key("a1", "anthropic"), key("a2", "anthropic"), key("a3", "anthropic")];
  const policy = { apiCreditGrants: [grant("g", ["a1"], { expiresAt: NOW + 2 * DAY }), grant("old", ["a3"], { expiresAt: NOW - DAY })] } as Policy;
  const summary = providerCredit({ accounts, policy }, "anthropic", NOW);
  assert.deepEqual([summary.leftUsd, summary.unknownPools, summary.nextExpiry, summary.lostUsd, summary.lostUnknownPools, summary.unsetKeys], [null, 1, NOW + 2 * DAY, 0, 1, 1]);
  const shown = providerCreditText(summary);
  assert.equal(shown.main, text("usage.credit.unknown"));
  assert.notEqual(shown.main, text("usage.credit.none"));
  assert.equal(shown.showExpiry, true);
  assert.deepEqual(shown.warnings, [text("usage.credit.lostUnknown", { count: 1 }), text("usage.credit.unset", { count: 1 })]);
  assert.equal(keyCreditView({ policy }, "a1", NOW).state, "unknown");
  assert.deepEqual([keyCreditView({ policy }, "a3", NOW).state, keyCreditView({ policy }, "a3", NOW).remainingUsd], ["expired", null]);
  assert.equal(keyCreditView({ policy }, "a2", NOW).state, "unset");
});

test("when only some pools have service status the total is labeled as confirmed-only", () => {
  const accounts = [key("a1", "anthropic"), key("a2", "anthropic"), key("a3", "anthropic"), key("a4", "anthropic")];
  const policy = { apiCreditGrants: [
    grant("known", ["a1"]), grant("newer", ["a2"], { expiresAt: NOW + DAY }),
    grant("lostKnown", ["a3"], { expiresAt: NOW - DAY }), grant("lostNew", ["a4"], { expiresAt: NOW - DAY }),
  ] } as Policy;
  // 서비스 상태는 일부 입력에만 있다(예: 방금 저장해 아직 계산되지 않은 입력).
  const statuses = [status("known", ["a1"], "active", 120), status("lostKnown", ["a3"], "expired", 40)];
  const summary = providerCredit({ accounts, policy, apiCreditGrants: statuses }, "anthropic", NOW);
  assert.deepEqual([summary.leftUsd, summary.unknownPools, summary.nextExpiry, summary.lostUsd, summary.lostUnknownPools], [120, 1, NOW + DAY, 40, 1]);
  const shown = providerCreditText(summary);
  assert.equal(shown.main, text("usage.credit.leftPartial", { amount: "$120.00", count: 1 }));
  assert.notEqual(shown.main, text("usage.credit.left", { amount: "$120.00" }));
  assert.deepEqual(shown.warnings, [text("usage.credit.lostPartial", { amount: "$40.00", count: 1 })]);
  // 모든 입력의 상태가 있으면 일반 문구다.
  const full = providerCreditText(providerCredit({ accounts: accounts.slice(0, 1), policy: { apiCreditGrants: [grant("known", ["a1"])] } as Policy, apiCreditGrants: statuses }, "anthropic", NOW));
  assert.deepEqual([full.main, full.warnings], [text("usage.credit.left", { amount: "$120.00" }), []]);
  // 진행 중 입력이 없으면 '없음'이다.
  assert.equal(providerCreditText(providerCredit({ accounts: [key("x", "anthropic")], policy: { apiCreditGrants: [] } as unknown as Policy }, "anthropic", NOW)).main, text("usage.credit.none"));
});

test("spend-ahead status names the provider switch, the missing credit, and prefers the running pool's reason", () => {
  const accounts = [key("a1", "anthropic"), key("o1", "openai")];
  const on = { balanceApiCredit: true, allocationMode: "expiry-balanced", useApiCreditAfterLimit: true, useOpenaiApiAfterLimit: false } as Policy;
  const grants = [grant("old", ["a1"], { expiresAt: NOW - DAY }), grant("new", ["a1"]), grant("oa", ["o1"])];
  const statuses = [status("old", ["a1"], "expired", 10, "GRANT_EXPIRED"), status("new", ["a1"], "active", 100, "AHEAD_OF_PACE"), status("oa", ["o1"], "active", 50, "OPT_IN_OFF")];
  const source = { accounts, policy: { ...on, apiCreditGrants: grants }, apiCreditGrants: statuses };
  assert.deepEqual(balanceStatus(source, "anthropic"), { allowed: false, reason: "AHEAD_OF_PACE", waiting: true });
  assert.deepEqual(balanceStatus(source, "openai"), { allowed: false, reason: "PROVIDER_OFF", waiting: false });
  assert.deepEqual(balanceStatus({ ...source, policy: { ...source.policy, useOpenaiApiAfterLimit: true }, apiCreditGrants: [...statuses.slice(0, 2), status("oa", ["o1"], "active", 50)] }, "openai"), { allowed: true });
  assert.deepEqual(balanceStatus({ accounts, policy: { ...on, apiCreditGrants: [] } }, "anthropic"), { allowed: false, reason: "GRANT_NONE", waiting: false });
  assert.deepEqual(balanceStatus({ accounts, policy: { ...on, allocationMode: "smart", apiCreditGrants: grants }, apiCreditGrants: statuses }, "anthropic"), { allowed: false, reason: "MODE", waiting: false });
  assert.equal(balanceStatus({ accounts: [key("a1", "anthropic")], policy: on }, "openai"), null);
});
