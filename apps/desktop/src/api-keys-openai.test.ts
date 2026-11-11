import { test } from "node:test";
import assert from "node:assert/strict";
import { apiAccounts, apiCreditNotes, apiCreditOverview, apiCreditUsage, apiKeyAddParams, apiKeyTotals, paidPolicyPatch, utcPeriodStart } from "./api-credit.ts";
import type { PaidForm } from "./api-credit.ts";
import type { AccountQuotaSummary, Policy } from "./types.ts";

const NOW = Date.UTC(2026, 10, 1, 12);
const START = utcPeriodStart(NOW);
const state = (usedUsd: number, unmeteredRequests = 0) => ({ periodStart: START, usedUsd, unmeteredRequests, updatedAt: NOW });
const key = (id: string, provider: string, usedUsd: number) => ({ id, tool: "api", provider, apiCredit: state(usedUsd) });
const summary = (id: string, provider: "anthropic" | "openai" | undefined, kind: string, usedUsd: number, limitUsd: number, limitReached = false): AccountQuotaSummary => ({
  accountIds: [id], kind, until: null, models: [], label: null, rate: false,
  apiCredit: { active: kind === "api-credit", usedUsd, limitUsd, limitReached, unmetered: false, ...(provider ? { provider } : {}) },
} as AccountQuotaSummary);

test("adding an OpenAI key sends provider openai; a Claude key keeps the old payload without provider", () => {
  assert.deepEqual(apiKeyAddParams({ provider: "openai", label: " Codex ", key: " sk-x ", confirmed: true }), { label: "Codex", key: "sk-x", confirmedNoAutoReload: true, provider: "openai" });
  const claude = apiKeyAddParams({ provider: "anthropic", label: "Max", key: "sk-ant-x", confirmed: true });
  assert.deepEqual(claude, { label: "Max", key: "sk-ant-x", confirmedNoAutoReload: true });
  assert.ok(!("provider" in claude));
});

test("monthly totals never mix keys of the other provider, and each provider adds only its own removed ledger", () => {
  const accounts = [key("c1", "anthropic", 40), key("o1", "openai", 7.5), key("o2", "openai", 2.5), { id: "legacy", tool: "api", apiCredit: state(1) }, { id: "sub", tool: "codex", provider: "openai", apiCredit: null }];
  assert.deepEqual(apiAccounts(accounts, "openai").map(account => account.id), ["o1", "o2"]);
  assert.deepEqual(apiAccounts(accounts, "anthropic").map(account => account.id), ["c1", "legacy"]);
  assert.equal(apiCreditUsage(accounts, NOW, state(100), "anthropic").usedUsd, 141);
  assert.equal(apiCreditUsage(accounts, NOW, state(5), "openai").usedUsd, 15);
  assert.equal(apiCreditUsage(accounts, NOW, null, "openai").usedUsd, 10);
  const totals = apiKeyTotals({ accounts, generatedAt: NOW, apiCreditRemoved: state(100), openaiApiRemoved: state(5), policy: { apiCreditMonthlyLimitUsd: 300 } as Policy });
  assert.equal(totals.anthropic.usedUsd, 141);
  assert.equal(totals.anthropic.limitUsd, 300);
  assert.equal(totals.openai.usedUsd, 15);
  assert.equal(totals.openai.limitUsd, 50);
  assert.equal(totals.openai.keys, 2);
});

test("an OpenAI summary is invisible to the Claude overview and the other way round", () => {
  const accounts = [key("c1", "anthropic", 150), key("o1", "openai", 30)];
  const summaries = [summary("c1", undefined, "resting", 150, 200), summary("o1", "openai", "api-credit", 30, 50)];
  const openai = apiCreditOverview(accounts, summaries, NOW, null, "openai");
  const claude = apiCreditOverview(accounts, summaries, NOW, null, "anthropic");
  assert.equal(openai.active, true);
  assert.match(openai.label ?? "", /\$30\.00 \/ \$50/);
  assert.equal(claude.active, false);
  assert.equal(claude.label, null);
  // 요약의 accountIds가 다른 공급자 키를 가리켜도 그 공급자의 개요에 끼지 않는다.
  const wrong = [summary("o1", "anthropic", "api-credit", 30, 200)];
  assert.equal(apiCreditOverview(accounts, wrong, NOW, null, "anthropic").label, null);
});

test("popover notes map each provider to its own card with its own total", () => {
  const accounts = [key("c1", "anthropic", 190), key("o1", "openai", 12)];
  const notes = apiCreditNotes({
    accounts, generatedAt: NOW, apiCreditRemoved: null, openaiApiRemoved: state(3),
    quotaSummaries: [summary("c1", "anthropic", "available", 190, 200), summary("o1", "openai", "api-credit", 12, 50)],
  });
  assert.deepEqual(notes.map(note => note.provider), ["openai"]);
  assert.match(notes[0].label, /\$15\.00 \/ \$50/);
  assert.equal(notes[0].active, true);
  const both = apiCreditNotes({
    accounts: [key("c1", "anthropic", 190), key("o1", "openai", 50)], generatedAt: NOW, apiCreditRemoved: null, openaiApiRemoved: null,
    quotaSummaries: [summary("c1", "anthropic", "api-credit", 190, 200), summary("o1", "openai", "resting", 50, 50, true)],
  });
  assert.deepEqual(both.map(note => note.provider), ["anthropic", "openai"]);
  assert.match(both[0].label, /\$190\.00 \/ \$200/);
  assert.match(both[1].label, /\$50\.00 \/ \$50/);
  assert.notEqual(both[0].reasonKey, both[1].reasonKey);
});

test("popover Claude note never divides summed key spend by the per-key cap, while OpenAI keeps its combined cap", () => {
  const accounts = [key("c1", "anthropic", 150), key("c2", "anthropic", 140), key("o1", "openai", 20), key("o2", "openai", 25)];
  const notes = apiCreditNotes({
    accounts, generatedAt: NOW, apiCreditRemoved: state(80), openaiApiRemoved: null,
    quotaSummaries: [summary("c1", "anthropic", "available", 150, 200), summary("c2", "anthropic", "api-credit", 140, 200), summary("o1", "openai", "api-credit", 20, 50), summary("o2", "openai", "api-credit", 25, 50)],
  });
  const claude = notes.find(note => note.provider === "anthropic");
  assert.match(claude?.label ?? "", /\$140\.00 \/ \$200/);
  assert.doesNotMatch(claude?.label ?? "", /\$370|\$290/);
  assert.match(notes.find(note => note.provider === "openai")?.label ?? "", /\$45\.00 \/ \$50/);
});

const form = (patch: Partial<PaidForm> = {}): PaidForm => ({ credits: false, extra: false, apiEnabled: false, apiLimit: "200", openaiEnabled: false, openaiLimit: "50", ...patch });
const base = { revision: 3 } as Policy;

test("settings send the OpenAI fields independently of the Claude fields, and only when changed", () => {
  const same = paidPolicyPatch(base, form());
  assert.equal(same.changed, false);
  assert.deepEqual(same.patch, { useCreditsAfterLimit: false, useExtraUsageAfterLimit: false });

  const openaiOnly = paidPolicyPatch(base, form({ openaiEnabled: true, openaiLimit: "75" }));
  assert.equal(openaiOnly.changed, true);
  assert.deepEqual(openaiOnly.patch, { useCreditsAfterLimit: false, useExtraUsageAfterLimit: false, useOpenaiApiAfterLimit: true, openaiApiMonthlyLimitUsd: 75 });

  const claudeOnly = paidPolicyPatch(base, form({ apiEnabled: true, apiLimit: "120" }));
  assert.deepEqual(claudeOnly.patch, { useCreditsAfterLimit: false, useExtraUsageAfterLimit: false, useApiCreditAfterLimit: true, apiCreditMonthlyLimitUsd: 120 });

  const configured = { revision: 3, useOpenaiApiAfterLimit: true, openaiApiMonthlyLimitUsd: 80, useApiCreditAfterLimit: true, apiCreditMonthlyLimitUsd: 300 } as Policy;
  const turnedOffClaude = paidPolicyPatch(configured, form({ apiEnabled: false, apiLimit: "300", openaiEnabled: true, openaiLimit: "80" }));
  assert.deepEqual(turnedOffClaude.patch, { useCreditsAfterLimit: false, useExtraUsageAfterLimit: false, useApiCreditAfterLimit: false, apiCreditMonthlyLimitUsd: 300 });
});

test("the OpenAI cap accepts 1..100000 only and an invalid value is never sent", () => {
  for (const bad of ["", "0", "0.5", "100001", "abc", "-5"]) {
    const result = paidPolicyPatch(base, form({ openaiEnabled: true, openaiLimit: bad }));
    assert.equal(result.openaiLimitValid, false, bad);
  }
  for (const good of ["1", "50", "100000"]) assert.equal(paidPolicyPatch(base, form({ openaiLimit: good })).openaiLimitValid, true, good);
  const invalid = paidPolicyPatch(base, form({ openaiEnabled: true, openaiLimit: "0" }));
  assert.ok(!("openaiApiMonthlyLimitUsd" in invalid.patch));
});
