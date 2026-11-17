import { test } from "node:test";
import assert from "node:assert/strict";
import { billingAccounts, billingFormFrom, billingPatch, creditSignature, draftRevision, estimateView, grantFormFrom, grantPatch, keyGrantForm, keyGrantOf, keyGrantPatch, localInputToMs, msToLocalInput, proactiveBlockKey, sharedConsentNeeded } from "./api-credit-grant.ts";
import { allocationModeOf } from "./allocation-mode.ts";
import type { ApiCreditBillingStatus, ApiCreditGrantPolicy } from "./types.ts";

const NOW = new Date(2026, 9, 31, 12, 0).getTime();
const DAY = 86_400_000;
const form = (patch: Partial<Parameters<typeof grantPatch>[0]> = {}) => ({ id: null as string | null, accountIds: ["api-1"], billingAccountId: null as string | null, budget: "120.5", startsAt: msToLocalInput(NOW - DAY), expiresAt: msToLocalInput(NOW + 20 * DAY), confirmed: true, expiryMode: "exact" as const, estimateConfirmedFor: null as number | null, ...patch });

test("unknown or future modes fall back to smart, known modes pass through", () => {
  assert.equal(allocationModeOf(undefined), "smart");
  assert.equal(allocationModeOf("something-new"), "smart");
  assert.equal(allocationModeOf("priority"), "priority");
  assert.equal(allocationModeOf("expiry-balanced"), "expiry-balanced");
});

test("a new pool is sent without an id and with its keys, so its spend starts at zero", () => {
  const result = grantPatch(form({ accountIds: ["api-1", "api-2"] }), NOW);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.equal(result.patch.budgetUsd, 120.5);
  assert.equal(result.patch.confirmed, true);
  assert.deepEqual(result.patch.accountIds, ["api-1", "api-2"]);
  assert.equal("id" in result.patch, false);
  assert.equal(result.patch.expiresAt, localInputToMs(msToLocalInput(NOW + 20 * DAY)));
});

test("a pool without any chosen key is rejected before reaching the service", () => {
  const result = grantPatch(form({ accountIds: [] }), NOW);
  assert.deepEqual(result, { ok: false, error: "keys" });
});

test("grant input is rejected the same way the service rejects it", () => {
  const error = (patch: Partial<Parameters<typeof grantPatch>[0]>) => { const result = grantPatch(form(patch), NOW); return result.ok ? null : result.error; };
  assert.equal(error({ budget: "" }), "budget");
  assert.equal(error({ budget: "0" }), "budget");
  assert.equal(error({ budget: "0.001" }), "budget");
  assert.equal(error({ budget: "-3" }), "budget");
  assert.equal(error({ budget: "1000001" }), "budget");
  assert.equal(error({ budget: "abc" }), "budget");
  assert.equal(error({ startsAt: "" }), "start");
  assert.equal(error({ expiresAt: "" }), "expiry");
  assert.equal(error({ expiresAt: msToLocalInput(NOW - 60_000) }), "past");
  assert.equal(error({ startsAt: msToLocalInput(NOW + 30 * DAY) }), "range");
  assert.equal(error({ expiresAt: msToLocalInput(NOW + 400 * DAY) }), "tooLong");
  assert.equal(error({ startsAt: msToLocalInput(NOW - 365 * DAY), expiresAt: msToLocalInput(NOW + 2 * DAY) }), "tooLong");
  assert.equal(error({ confirmed: false }), "confirm");
});

const pool = (over: Partial<ApiCreditGrantPolicy> = {}): ApiCreditGrantPolicy => ({ id: "g1", accountIds: ["api-1", "api-2"], budgetUsd: 100, startsAt: NOW - 5 * DAY, expiresAt: NOW + 10 * DAY, confirmedAt: NOW - 5 * DAY, source: "manual", ...over });

test("editing a pool keeps its id and its saved budget, never the remaining amount, so spend is not subtracted twice", () => {
  // 저장 100 중 12.4를 이미 썼어도 폼에는 남은 87.6이 아니라 저장된 100을 채워야 한다. 같은 id 저장은 서비스가 사용액을 또 빼기 때문이다.
  const filled = grantFormFrom(pool(), [], NOW, ["api-1", "api-2"]);
  assert.equal(filled.id, "g1");
  assert.equal(filled.budget, "100");
  assert.equal(filled.confirmed, false);
  const result = grantPatch({ ...filled, confirmed: true }, NOW);
  assert.ok(result.ok);
  if (result.ok) { assert.equal(result.patch.id, "g1"); assert.equal(result.patch.budgetUsd, 100); assert.deepEqual(result.patch.accountIds, ["api-1", "api-2"]); }
  // 새 풀 폼은 id 없이 비어 있다.
  const blank = grantFormFrom(null, [], NOW, ["api-1"]);
  assert.equal(blank.id, null);
  assert.deepEqual(blank.accountIds, []);
  assert.equal(blank.budget, "");
});

test("editing a pool never sends a removed key's id, which the service would reject", () => {
  const filled = grantFormFrom(pool({ accountIds: ["api-1", "gone"] }), [], NOW, ["api-1", "api-2"]);
  assert.deepEqual(filled.accountIds, ["api-1"]);
  // 등록된 키가 하나도 안 남았으면 키 미선택으로 막힌다.
  const empty = grantFormFrom(pool({ accountIds: ["gone"] }), [], NOW, ["api-1"]);
  const result = grantPatch({ ...empty, confirmed: true }, NOW);
  assert.deepEqual(result, { ok: false, error: "keys" });
});

test("a new key credit holds only the selected key and is sent without an id, never picking other keys", () => {
  const blank = keyGrantForm(null, "api-2", [], NOW, ["api-1", "api-2", "api-3"]);
  assert.deepEqual(blank.accountIds, ["api-2"]);
  assert.equal(blank.id, null);
  assert.equal(blank.expiryMode, "exact");
  const result = keyGrantPatch({ form: { ...blank, budget: "50", expiresAt: msToLocalInput(NOW + 10 * DAY), confirmed: true }, accountId: "api-2", original: null, sharedConsent: false }, NOW);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.deepEqual(result.patch.accountIds, ["api-2"]);
  assert.equal("id" in result.patch, false);
});

test("a key is found in its own credit only, and a neighbour's single-key credit is not matched", () => {
  const grants = [pool({ id: "g1", accountIds: ["api-1"] }), pool({ id: "g2", accountIds: ["api-2"] })];
  assert.equal(keyGrantOf(grants, "api-2")?.id, "g2");
  assert.equal(keyGrantOf(grants, "api-3"), null);
  assert.equal(keyGrantOf(undefined, "api-1"), null);
});

test("editing a single-key credit keeps its id, saved budget and exact future start", () => {
  const grant = pool({ id: "solo", accountIds: ["api-2"], startsAt: NOW + 3 * DAY, expiresAt: NOW + 20 * DAY });
  const filled = keyGrantForm(grant, "api-2", [], NOW, ["api-1", "api-2"]);
  assert.equal(filled.id, "solo");
  assert.equal(filled.budget, "100");
  const result = keyGrantPatch({ form: { ...filled, confirmed: true }, accountId: "api-2", original: grant, sharedConsent: false }, NOW);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.deepEqual(result.patch, { id: "solo", accountIds: ["api-2"], budgetUsd: 100, startsAt: grant.startsAt, expiresAt: localInputToMs(msToLocalInput(grant.expiresAt)), confirmed: true });
});

test("a shared legacy credit keeps its members and id, and cannot change without explicit consent", () => {
  const grant = pool({ id: "legacy", accountIds: ["api-1", "api-2"] });
  const filled = keyGrantForm(grant, "api-2", [], NOW, ["api-1", "api-2"]);
  assert.deepEqual(filled.accountIds, ["api-1", "api-2"]);
  assert.equal(sharedConsentNeeded(grant.accountIds, filled.accountIds), true);
  const ask = (sharedConsent: boolean) => keyGrantPatch({ form: { ...filled, confirmed: true }, accountId: "api-2", original: grant, sharedConsent }, NOW);
  assert.deepEqual(ask(false), { ok: false, error: "shared" });
  const ok = ask(true);
  assert.ok(ok.ok);
  if (ok.ok) { assert.equal(ok.patch.id, "legacy"); assert.deepEqual(ok.patch.accountIds, ["api-1", "api-2"]); assert.equal(ok.patch.budgetUsd, 100); }
});

test("the selected key stays in the credit even if it is missing from the stored members, and a form without it is rejected", () => {
  const grant = pool({ id: "g", accountIds: ["api-1"] });
  assert.deepEqual(keyGrantForm(grant, "api-2", [], NOW, ["api-1", "api-2"]).accountIds, ["api-2", "api-1"]);
  const dropped = { ...keyGrantForm(grant, "api-1", [], NOW, ["api-1"]), confirmed: true };
  assert.deepEqual(keyGrantPatch({ form: dropped, accountId: "api-2", original: grant, sharedConsent: true }, NOW), { ok: false, error: "keys" });
});

test("a draft keeps the revision it was made from unless only unrelated policy changed, and never silently takes a newer revision after a real change", () => {
  const grant = pool({ id: "g", accountIds: ["api-1"] });
  const base = { revision: 4, signature: creditSignature(grant, []) };
  // 다른 설정만 바뀜(자동 배정 스위치 등): 이어 받아도 이 키 입력은 그대로다.
  assert.deepEqual(draftRevision(base, { revision: 5, signature: creditSignature(grant, []) }), { revision: 5, conflict: false });
  // 이 키의 입력이 다른 곳에서 바뀜: 폼을 만든 revision을 그대로 보내 서비스가 거절하게 한다.
  assert.deepEqual(draftRevision(base, { revision: 5, signature: creditSignature({ ...grant, budgetUsd: 1 }, []) }), { revision: 4, conflict: true });
  // 입력이 지워지거나 새로 생김도 같다.
  assert.equal(draftRevision(base, { revision: 6, signature: creditSignature(null, []) }).conflict, true);
  assert.equal(draftRevision({ revision: 4, signature: creditSignature(null, []) }, { revision: 5, signature: creditSignature(grant, []) }).revision, 4);
});

test("editing a future pool preserves its start through form refill and patch", () => {
  const grant = pool({ id: "future", startsAt: NOW + 3 * DAY, expiresAt: NOW + 20 * DAY, confirmedAt: NOW });
  const filled = grantFormFrom(grant, [], NOW, ["api-1", "api-2"]);
  const result = grantPatch({ ...filled, budget: "80", expiresAt: msToLocalInput(NOW + 21 * DAY), confirmed: true }, NOW);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.equal(result.patch.startsAt, grant.startsAt);
  assert.equal(result.patch.expiresAt, NOW + 21 * DAY);
  assert.equal(result.patch.budgetUsd, 80);
});

test("an unknown proactive block code from a newer service has no dedicated text, known ones do", () => {
  assert.ok(proactiveBlockKey("GRANT_EXPIRED"));
  assert.equal(proactiveBlockKey("FUTURE_CODE"), null);
  assert.equal(proactiveBlockKey(null), null);
});

const billing = (over: Partial<ApiCreditBillingStatus> = {}): ApiCreditBillingStatus => ({ accountId: "claude-1", cycle: "monthly", nextBillingAt: NOW + 12 * DAY, estimatedExpiresAt: NOW + 12 * DAY, estimateBlockedBy: null, ...over });
const estimateForm = (over: Partial<Parameters<typeof grantPatch>[0]> = {}) => form({ expiresAt: "", expiryMode: "billing-estimate", billingAccountId: "claude-1", estimateConfirmedFor: NOW + 12 * DAY, ...over });
const errorOf = (input: Parameters<typeof grantPatch>[0], status?: ApiCreditBillingStatus | null) => { const result = grantPatch(input, NOW, status ? [status] : []); return result.ok ? null : result.error; };

test("a monthly billing estimate sends the chosen billing account's estimated expiry with its source and the separate consent", () => {
  const result = grantPatch(estimateForm(), NOW, [billing()]);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.equal(result.patch.expiresAt, NOW + 12 * DAY);
  assert.equal(result.patch.expirySource, "billing-estimate");
  assert.equal(result.patch.estimateConfirmed, true);
  assert.equal(result.patch.budgetUsd, 120.5);
});

test("a manually typed exact expiry keeps the old patch shape, so it is never labeled an estimate", () => {
  const result = grantPatch(form(), NOW, [billing()]);
  assert.ok(result.ok);
  if (!result.ok) return;
  assert.equal("expirySource" in result.patch, false);
  assert.equal("estimateConfirmed" in result.patch, false);
  assert.equal("billingAccountId" in result.patch, false);
});

test("with several billing accounts the estimate uses the selected account's expiry, not another's", () => {
  const billings = [billing({ accountId: "claude-1", estimatedExpiresAt: NOW + 12 * DAY }), billing({ accountId: "claude-2", nextBillingAt: NOW + 20 * DAY, estimatedExpiresAt: NOW + 20 * DAY })];
  const second = grantPatch(estimateForm({ billingAccountId: "claude-2", estimateConfirmedFor: NOW + 20 * DAY }), NOW, billings);
  assert.ok(second.ok);
  if (second.ok) { assert.equal(second.patch.expiresAt, NOW + 20 * DAY); assert.equal(second.patch.billingAccountId, "claude-2"); }
  // 다른 계정의 추정에 동의한 채 계정을 바꾸면 동의가 맞지 않아 저장되지 않는다.
  assert.equal(grantPatch(estimateForm({ billingAccountId: "claude-2" }), NOW, billings).ok, false);
  // 선택한 계정에 결제 정보가 없으면 다른 계정 것으로 대신하지 않는다.
  assert.deepEqual(grantPatch(estimateForm({ billingAccountId: "claude-3" }), NOW, billings), { ok: false, error: "estimateNone" });
  assert.deepEqual(grantPatch(estimateForm({ billingAccountId: null }), NOW, billings), { ok: false, error: "estimateNone" });
});

test("the estimate needs the balance confirmation and its own consent for the exact estimate being shown", () => {
  assert.equal(errorOf(estimateForm({ confirmed: false }), billing()), "confirm");
  assert.equal(errorOf(estimateForm({ estimateConfirmedFor: null }), billing()), "estimateConfirm");
  // 동의한 뒤 결제일이 바뀌어 추정이 달라지면 이전 동의로는 저장되지 않는다.
  assert.equal(errorOf(estimateForm(), billing({ estimatedExpiresAt: NOW + 13 * DAY })), "estimateConfirm");
});

test("annual billing, a passed date, a missing or invalid account never produce an estimate or a rolled-forward renewal", () => {
  assert.equal(errorOf(estimateForm(), billing({ cycle: "annual", estimatedExpiresAt: null, estimateBlockedBy: "ANNUAL" })), "estimateAnnual");
  // 서비스가 값을 줬더라도 연 결제면 월 만료로 쓰지 않는다.
  assert.equal(errorOf(estimateForm(), billing({ cycle: "annual" })), "estimateAnnual");
  assert.equal(errorOf(estimateForm(), billing({ estimatedExpiresAt: null, estimateBlockedBy: "DATE_PASSED" })), "estimatePassed");
  // 폴링 사이에 결제 시각이 지나도 지난 시각을 다음 달로 밀지 않는다.
  assert.equal(errorOf(estimateForm(), billing({ estimatedExpiresAt: NOW - 1 })), "estimatePassed");
  assert.equal(errorOf(estimateForm(), billing({ estimatedExpiresAt: NOW })), "estimatePassed");
  assert.equal(errorOf(estimateForm(), billing({ estimatedExpiresAt: null, estimateBlockedBy: "ACCOUNT_INVALID" })), "estimateAccount");
  assert.equal(errorOf(estimateForm(), null), "estimateNone");
  assert.equal(errorOf(estimateForm(), undefined), "estimateNone");
});

test("estimate and exact date follow the same range rules, including the 366 day limit and a start after the estimate", () => {
  assert.equal(errorOf(estimateForm({ startsAt: msToLocalInput(NOW + 20 * DAY) }), billing()), "range");
  const far = billing({ nextBillingAt: NOW + 400 * DAY, estimatedExpiresAt: NOW + 400 * DAY });
  assert.equal(errorOf(estimateForm({ estimateConfirmedFor: NOW + 400 * DAY }), far), "tooLong");
});

test("estimateView reports why a billing date cannot be used", () => {
  assert.deepEqual(estimateView(billing(), NOW), { expiresAt: NOW + 12 * DAY, blockedBy: null });
  assert.equal(estimateView(billing({ cycle: "annual" }), NOW).blockedBy, "ANNUAL");
  assert.equal(estimateView(billing({ estimatedExpiresAt: null }), NOW).blockedBy, "DATE_PASSED");
  assert.equal(estimateView(undefined, NOW).blockedBy, "NONE");
});

test("refilling a billing-estimate pool never copies the estimate into the exact date box, keeps a future start and the billing account", () => {
  const grant = pool({ id: "e1", budgetUsd: 50, startsAt: NOW + 2 * DAY, expiresAt: NOW + 12 * DAY, confirmedAt: NOW, expirySource: "billing-estimate", billingEstimate: { accountId: "claude-1", nextBillingAt: NOW + 12 * DAY } });
  const keyIds = ["api-1", "api-2"];
  const usable = grantFormFrom(grant, [billing()], NOW, keyIds);
  assert.equal(usable.expiryMode, "billing-estimate");
  assert.equal(usable.billingAccountId, "claude-1");
  assert.equal(usable.expiresAt, "");
  assert.equal(usable.estimateConfirmedFor, null);
  assert.equal(usable.startsAt, msToLocalInput(grant.startsAt));
  const annual = grantFormFrom(grant, [billing({ cycle: "annual", estimatedExpiresAt: null, estimateBlockedBy: "ANNUAL" })], NOW, keyIds);
  assert.equal(annual.expiryMode, "exact");
  assert.equal(annual.expiresAt, "");
  // 정확한 날짜 입력은 이전처럼 채운다.
  assert.equal(grantFormFrom({ ...grant, expirySource: "exact" }, [], NOW, keyIds).expiresAt, msToLocalInput(grant.expiresAt));
  assert.equal(grantFormFrom({ ...grant, expirySource: undefined }, [], NOW, keyIds).expiryMode, "exact");
});

const accounts = [
  { id: "claude-1", provider: "anthropic", tool: "claude" },
  { id: "claude-omp", provider: "anthropic", tool: "omp" },
  { id: "api-1", provider: "anthropic", tool: "api" },
  { id: "codex-1", provider: "openai", tool: "codex" },
  { id: "goog-1", provider: "google", tool: "omp" },
];

test("only Anthropic subscription accounts can hold billing info, never API keys or other providers", () => {
  assert.deepEqual(billingAccounts(accounts).map(account => account.id), ["claude-1"]);
});

test("billing input becomes a patch with the exact typed instant, and rejects unusable accounts and dates", () => {
  const eligible = billingAccounts(accounts);
  const input = { accountId: "claude-1", cycle: "monthly" as const, nextBillingAt: msToLocalInput(NOW + 20 * DAY) };
  const ok = billingPatch(input, eligible, NOW);
  assert.ok(ok.ok);
  if (ok.ok) assert.deepEqual(ok.patch, { accountId: "claude-1", cycle: "monthly", nextBillingAt: localInputToMs(input.nextBillingAt) });
  const error = (patch: Partial<typeof input>) => { const result = billingPatch({ ...input, ...patch }, eligible, NOW); return result.ok ? null : result.error; };
  assert.equal(error({ accountId: "api-1" }), "account");
  assert.equal(error({ accountId: "codex-1" }), "account");
  assert.equal(error({ accountId: "" }), "account");
  assert.equal(error({ nextBillingAt: "" }), "date");
  assert.equal(error({ nextBillingAt: "2026-10-31" }), "date");
  assert.equal(error({ nextBillingAt: msToLocalInput(NOW - 60_000) }), "past");
  assert.equal(error({ nextBillingAt: msToLocalInput(NOW + 36 * DAY) }), "tooFar");
  assert.equal(error({ nextBillingAt: msToLocalInput(NOW + 34 * DAY) }), null);
  assert.equal(error({ cycle: "annual", nextBillingAt: msToLocalInput(NOW + 300 * DAY) }), null);
  assert.equal(error({ cycle: "annual", nextBillingAt: msToLocalInput(NOW + 368 * DAY) }), "tooFar");
});

test("billing form refills from the saved value only, and picks the sole eligible account", () => {
  const eligible = [{ id: "claude-1" }];
  assert.deepEqual(billingFormFrom(null, eligible), { accountId: "claude-1", cycle: "monthly", nextBillingAt: "" });
  assert.equal(billingFormFrom(null, [{ id: "a" }, { id: "b" }]).accountId, "");
  assert.deepEqual(billingFormFrom({ accountId: "b", cycle: "annual", nextBillingAt: NOW + DAY, updatedAt: NOW }, eligible), { accountId: "b", cycle: "annual", nextBillingAt: msToLocalInput(NOW + DAY) });
});

test("month-end billing dates are sent as typed, with no calendar rolling or end-of-day guess", () => {
  const jan31 = new Date(2027, 0, 31, 9, 30).getTime();
  const result = billingPatch({ accountId: "claude-1", cycle: "monthly", nextBillingAt: "2027-01-31T09:30" }, billingAccounts(accounts), new Date(2027, 0, 5).getTime());
  assert.ok(result.ok);
  if (result.ok) assert.equal(result.patch.nextBillingAt, jan31);
});

