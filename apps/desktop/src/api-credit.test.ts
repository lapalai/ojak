import { test } from "node:test";
import assert from "node:assert/strict";
import { apiAccountUsage, apiCreditAmount, apiCreditLabel, apiCreditOverview, apiCreditUsage, subscriptionAccounts, utcPeriodStart } from "./api-credit.ts";
import { dictionaries } from "./i18n.ts";
import type { Account, AccountQuotaSummary } from "./types.ts";

const LOCALES = ["en", "ko", "id"] as const;
const NOW = Date.UTC(2026, 9, 30, 12);
const credit = (patch: Partial<NonNullable<AccountQuotaSummary["apiCredit"]>> = {}): AccountQuotaSummary => ({
  accountIds: ["key"], kind: "api-credit", until: null, models: [], label: null, rate: false,
  apiCredit: { active: true, usedUsd: 12.4, limitUsd: 200, limitReached: false, unmetered: false, ...patch },
});

test("api-credit summary text shows the estimated amount in the current language", () => {
  assert.equal(apiCreditLabel(credit()), `${dictionaries.en["usage.verdict.api-credit"]} · $12.40 / $200 (est.)`);
  assert.equal(apiCreditAmount(12.4, 200), "$12.40 / $200 (est.)");
});

test("api-credit label is hidden while the key is idle, and names the cap once reached", () => {
  assert.equal(apiCreditLabel({ ...credit({ active: false }), kind: "available" }), null);
  assert.equal(apiCreditLabel(undefined), null);
  assert.match(apiCreditLabel(credit({ active: false, limitReached: true, usedUsd: 200 })) ?? "", /API credit cap reached · \$200\.00 \/ \$200/);
});

test("with several keys the status line shows the combined amount and prefers the key in use", () => {
  const start = utcPeriodStart(NOW);
  const key = (id: string, usedUsd: number) => ({ id, tool: "api", apiCredit: { periodStart: start, usedUsd, unmeteredRequests: 0, updatedAt: NOW } });
  const summary = (id: string, kind: string, usedUsd: number, limitReached = false): AccountQuotaSummary => ({
    accountIds: [id], kind, until: null, models: [], label: null, rate: false,
    apiCredit: { active: kind === "api-credit", usedUsd, limitUsd: 200, limitReached, unmetered: false },
  } as AccountQuotaSummary);
  const accounts = [key("a", 150), key("b", 40), { id: "s", tool: "claude", apiCredit: null }];
  const view = apiCreditOverview(accounts, [summary("a", "available", 150), summary("b", "api-credit", 40)], NOW);
  assert.equal(view.active, true);
  assert.equal(view.label, `${dictionaries.en["usage.verdict.api-credit"]} · $190.00 / $200 (est.)`);
  const capped = apiCreditOverview([key("a", 150), key("b", 150)], [summary("a", "resting", 150, true), summary("b", "resting", 150, true)], NOW);
  assert.match(capped.label ?? "", /cap reached · \$300\.00 \/ \$200/);
});

test("api key accounts are kept out of subscription lists", () => {
  const accounts = [{ id: "a", tool: "claude" }, { id: "k", tool: "api" }, { id: "c", tool: "codex" }];
  assert.deepEqual(subscriptionAccounts(accounts).map(account => account.id), ["a", "c"]);
});

test("monthly usage restarts when the recorded period is not the current UTC month", () => {
  const start = utcPeriodStart(NOW);
  assert.equal(start, Date.UTC(2026, 9, 1));
  const key = (periodStart: number, usedUsd: number, unmeteredRequests = 0) => ({ tool: "api", apiCredit: { periodStart, usedUsd, unmeteredRequests, updatedAt: NOW } }) satisfies Pick<Account, "tool" | "apiCredit">;
  assert.deepEqual(apiAccountUsage(key(start, 12.4, 2), NOW), { usedUsd: 12.4, unmeteredRequests: 2 });
  assert.deepEqual(apiAccountUsage(key(Date.UTC(2026, 8, 1), 90), NOW), { usedUsd: 0, unmeteredRequests: 0 });
  assert.deepEqual(apiAccountUsage({ apiCredit: null }, NOW), { usedUsd: 0, unmeteredRequests: 0 });
  assert.deepEqual(apiCreditUsage([key(start, 1.5, 1), key(start, 2.25), { tool: "claude", apiCredit: null }], NOW), { usedUsd: 3.75, unmeteredRequests: 1 });
});

test("removed keys' spend this month stays in the displayed total, other months do not", () => {
  const start = utcPeriodStart(NOW);
  const key = { id: "b", tool: "api", apiCredit: { periodStart: start, usedUsd: 60, unmeteredRequests: 0, updatedAt: NOW } };
  const removed = { periodStart: start, usedUsd: 150, unmeteredRequests: 2, updatedAt: NOW };
  assert.deepEqual(apiCreditUsage([key], NOW, removed), { usedUsd: 210, unmeteredRequests: 2 });
  assert.deepEqual(apiCreditUsage([key], NOW, { ...removed, periodStart: Date.UTC(2026, 8, 1) }), { usedUsd: 60, unmeteredRequests: 0 });
  assert.deepEqual(apiCreditUsage([key], NOW, null), { usedUsd: 60, unmeteredRequests: 0 });
  const summary = { accountIds: ["b"], kind: "resting", until: null, models: [], label: null, rate: false, apiCredit: { active: false, usedUsd: 60, limitUsd: 200, limitReached: true, unmetered: false } } as AccountQuotaSummary;
  assert.match(apiCreditOverview([key], [summary], NOW, removed).label ?? "", /cap reached · \$210\.00 \/ \$200/);
});

test("every API credit key exists in en, ko and id with the same placeholders", () => {
  const placeholders = (text: unknown) => (typeof text === "string" ? [...text.matchAll(/\{(\w+)\}/g)].map(match => match[1]).sort() : null);
  const keys = Object.keys(dictionaries.en).filter(key => /^(apiKey\.|policy\.apiCredit|policy\.openaiApi|usage\.apiCredit\.|usage\.openaiApi\.|usage\.(verdict|reason)\.(api-credit|openai-api)|error\.code\.API_)/.test(key));
  assert.ok(keys.length >= 30);
  for (const key of keys) {
    const reference = placeholders((dictionaries.en as Record<string, unknown>)[key]);
    for (const name of LOCALES) {
      const text = (dictionaries[name] as Record<string, unknown>)[key];
      assert.equal(typeof text, "string", `${name} is missing: ${key}`);
      assert.notEqual((text as string).trim(), "", `${name} is empty: ${key}`);
      assert.deepEqual(placeholders(text), reference, `${name} placeholders differ: ${key}`);
    }
  }
  for (const code of ["API_KEY_FORMAT", "API_KEY_INVALID", "API_KEY_CHECK_FAILED", "API_CREDIT_LIMIT", "API_CREDIT_CONFIRM_REQUIRED"]) {
    assert.ok(`error.code.${code}` in dictionaries.en);
  }
  assert.equal(dictionaries.ko["usage.verdict.api-credit"], "API 크레딧 사용 중");
  assert.equal(dictionaries.id["usage.verdict.api-credit"], "Pakai kredit API");
});
