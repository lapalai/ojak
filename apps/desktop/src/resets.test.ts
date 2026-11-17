import { test } from "node:test";
import assert from "node:assert/strict";
import { canUseReset, parseResetInventory, parseResetOperation, parseResetWebStatus, resetScopeKey, resetUseBlock, resetWebOffer, resetWebStale } from "./resets.ts";
import type { ResetCredit, ResetInventory, ResetOperation, ResetWebStatus } from "./resets.ts";

const operation = (state: ResetOperation["state"]): ResetOperation => ({ requestId: "request-a", accountId: "account-a", creditId: "credit-a", state, code: "result_unknown", startedAt: 1000, updatedAt: 1000 });
const credit = (over: Partial<ResetCredit> = {}): ResetCredit => ({ id: "credit-a", program: "cedar_ember", remainingCount: 2,
  usable: true, expiresAt: 200000, startsAt: null, clears: ["anthropic:5h"], requiresLimit: true, status: "available",
  resetType: "session", givesBackWeekly: false, blocking: [], unavailableReason: null, cooldownUntil: null, ...over });
const webStatus = (over: Partial<ResetWebStatus> = {}): ResetWebStatus => ({ accountId: "account-a", connected: true, state: "ready",
  lastAttemptAt: 1000, lastSuccessAt: 1000, nextRefreshAt: 301000, errorCode: null, ...over });
const inventory = (): ResetInventory => ({ accountId: "account-a", state: "known", availableCount: 2, redeemableCount: 2,
  observedAt: 1000, quoteId: "quote-a", operation: null, lookupReason: null, source: "oauth", web: null, credits: [credit()] });
const parsed = (over: Partial<ResetInventory> = {}) => parseResetInventory({ ...inventory(), ...over });
const webInventory = (over: Partial<ResetInventory> = {}) => parsed({ source: "web", web: webStatus(), quoteId: "quote-a", ...over });

test("unknown, unsupported and restricted inventory remain distinct from a confirmed zero balance", () => {
  const zero = { ...inventory(), availableCount: 0, redeemableCount: 0, credits: [] };
  assert.equal(parseResetInventory(zero).availableCount, 0);
  const unknown = { ...zero, state: "unknown", availableCount: null, redeemableCount: null, quoteId: null };
  assert.equal(parseResetInventory(unknown).availableCount, null);
  assert.equal(parseResetInventory({ ...unknown, state: "unsupported" }).state, "unsupported");
  const restricted = parseResetInventory({ ...unknown, state: "restricted", lookupReason: "surface" });
  assert.deepEqual([restricted.state, restricted.availableCount, restricted.redeemableCount, restricted.lookupReason], ["restricted", null, null, "surface"]);
  assert.throws(() => parseResetInventory({ ...unknown, state: "restricted", availableCount: 0, redeemableCount: 0 }));
  assert.throws(() => parseResetInventory({ ...zero, availableCount: null }));
  assert.throws(() => parseResetInventory({ ...zero, redeemableCount: -1 }));
  assert.throws(() => parseResetOperation({ ...operation("unknown"), state: "retry" }));
});

test("a restricted or unknown lookup never carries spend authority, even if the response still has a quote", () => {
  const offer = credit();
  for (const state of ["restricted", "unknown", "unsupported"] as const) {
    const value = parsed({ state, availableCount: null, redeemableCount: null, quoteId: "quote-a", credits: [offer] });
    assert.equal(value.quoteId, null, state);
    assert.equal(value.credits.length, 1, "returned offers stay visible as observations");
    assert.equal(canUseReset(value, value.credits[0], 1001), false, state);
  }
});

test("a service missing restriction metadata cannot report a trusted zero or authorize an old offer", () => {
  const { resetType: _t, givesBackWeekly: _g, blocking: _b, unavailableReason: _u, cooldownUntil: _c, ...legacy } = credit();
  assert.throws(() => parseResetInventory({ ...inventory(), lookupReason: undefined, availableCount: 0, redeemableCount: 0, credits: [] }));
  assert.throws(() => parseResetInventory({ ...inventory(), credits: [legacy] }));
  assert.equal(parsed({ credits: [credit({ resetType: "invented" as never })] }).credits[0].resetType, "unknown");
  assert.throws(() => parsed({ credits: [credit({ blocking: ["bad scope!"] })] }));
  assert.throws(() => parsed({ credits: [credit({ unavailableReason: "free text with spaces" })] }));
});

test("confirmation expires at the quote and provider boundaries and cannot use a foreign offer", () => {
  const value = parsed(); const item = value.credits[0];
  assert.equal(canUseReset(value, item, 121000), true);
  assert.equal(canUseReset(value, item, 121001), false);
  assert.equal(canUseReset(value, item, 999), false);
  item.expiresAt = 1001;
  assert.equal(canUseReset(value, item, 1001), false);
  item.expiresAt = null; item.startsAt = 1002;
  assert.equal(canUseReset(value, item, 1001), false);
  item.startsAt = null;
  assert.equal(canUseReset(value, { ...item }, 1001), false);
  assert.equal(canUseReset({ ...value, quoteId: null }, item, 1001), false);
  item.cooldownUntil = 1002;
  assert.equal(resetUseBlock(value, item, 1001), "cooldown");
  assert.equal(resetUseBlock(value, item, 1002), null);
});

test("unconfirmed mutation outcomes prevent any further use, including a remaining multi-use grant", () => {
  const value = parsed();
  for (const state of ["preparing", "submitted", "unknown"] as const) {
    assert.equal(canUseReset({ ...value, operation: operation(state) }, value.credits[0], 1001), false);
  }
  assert.equal(canUseReset({ ...value, state: "unknown" }, value.credits[0], 1001), false);
  value.credits[0].usable = false;
  assert.equal(canUseReset(value, value.credits[0], 1001), false);
});

test("every supported category is spendable only when its type and scopes are recognized", () => {
  const cases: [Partial<ResetCredit>, boolean][] = [
    [{ resetType: "session", clears: ["anthropic:5h"] }, true],
    [{ resetType: "weekly", clears: ["anthropic:7d"] }, true],
    [{ resetType: "full", clears: ["anthropic:5h", "anthropic:7d"] }, true],
    [{ resetType: "fable", givesBackWeekly: true, clears: ["anthropic:7d:fable"] }, true],
    [{ resetType: "provider", program: "codex_rate_limits", clears: ["openai-codex:rate_limits"] }, true],
    [{ resetType: "weekly", clears: ["anthropic:7d", "anthropic:reset:seven_day_cowork"] }, true],
    [{ resetType: "unknown" }, false],
    [{ resetType: "weekly", clears: [] }, false],
    [{ resetType: "weekly", clears: ["anthropic:reset:seven_day_mystery"] }, false],
    [{ resetType: "weekly", clears: ["anthropic:7d", "anthropic:reset:seven_day_mystery"] }, false],
  ];
  for (const [over, expected] of cases) {
    const value = parsed({ credits: [credit(over)] });
    assert.equal(canUseReset(value, value.credits[0], 1001), expected, JSON.stringify(over));
  }
});

test("Fable is a partial weekly reimbursement and its flag must agree with its type", () => {
  // A Fable grant that lost its flag, or a flagged grant of another type, is not described correctly and cannot be confirmed.
  for (const over of [{ resetType: "fable", givesBackWeekly: false }, { resetType: "weekly", givesBackWeekly: true }] as const) {
    const value = parsed({ credits: [credit({ ...over, clears: ["anthropic:7d:fable"] })] });
    assert.equal(resetUseBlock(value, value.credits[0], 1001), "unrecognized");
  }
  const fable = parsed({ credits: [credit({ resetType: "fable", givesBackWeekly: true, clears: ["anthropic:7d:fable"] })] });
  assert.equal(fable.credits[0].givesBackWeekly, true);
  assert.notEqual(resetScopeKey("anthropic:7d:fable"), resetScopeKey("anthropic:7d"), "Fable weekly usage is its own scope, not the full weekly limit");
});

test("offers with a service reason or blocking scopes are shown but cannot be confirmed", () => {
  for (const over of [{ unavailableReason: "surface" }, { unavailableReason: "blocked", blocking: ["anthropic:7d"] }, { blocking: ["anthropic:7d"] }]) {
    const value = parsed({ credits: [credit(over)] });
    assert.equal(canUseReset(value, value.credits[0], 1001), false);
  }
  assert.equal(parsed({ credits: [credit({ unavailableReason: "surface" })] }).credits[0].unavailableReason, "surface");
});

test("scope identifiers are recognized exactly; look-alikes are not", () => {
  assert.equal(resetScopeKey("anthropic:5h"), "session");
  assert.equal(resetScopeKey("anthropic:reset:seven_day_oauth_apps"), "oauthApps");
  assert.equal(resetScopeKey("constructor"), null);
  assert.equal(resetScopeKey("anthropic:7d "), null);
  assert.equal(resetScopeKey("openai-codex:rate_limits"), "codex");
});

test("a service that does not report the source or web status is rejected, never read as OAuth or not connected", () => {
  const { source: _s, ...noSource } = inventory();
  const { web: _w, ...noWeb } = inventory();
  assert.throws(() => parseResetInventory(noSource));
  assert.throws(() => parseResetInventory(noWeb));
  assert.throws(() => parsed({ source: "scrape" as never }));
  assert.throws(() => parsed({ source: "web", web: null }), "a web inventory needs a live connection");
  assert.throws(() => parsed({ source: "web", web: webStatus({ connected: false, state: "disconnected" }) }));
  assert.throws(() => parsed({ web: webStatus({ accountId: "account-b" }) }), "a status for another account is not attributed");
  assert.equal(parsed({ web: webStatus({ connected: false, state: "disconnected" }) }).web?.connected, false);
});

test("web status parsing rejects contradictions, unknown states and free-text error codes", () => {
  assert.equal(parseResetWebStatus(webStatus()).state, "ready");
  assert.throws(() => parseResetWebStatus({ ...webStatus(), state: "paused" }));
  assert.throws(() => parseResetWebStatus({ ...webStatus(), connected: false }));
  assert.throws(() => parseResetWebStatus({ ...webStatus({ connected: false, state: "disconnected" }), connected: true }));
  assert.throws(() => parseResetWebStatus({ ...webStatus(), errorCode: "free text with spaces" }));
  assert.throws(() => parseResetWebStatus({ ...webStatus(), lastSuccessAt: -1 }));
  assert.throws(() => parseResetWebStatus({ ...webStatus(), nextRefreshAt: undefined }));
  assert.equal(parseResetWebStatus(webStatus({ state: "login_required", errorCode: "WEB_LOGIN_REQUIRED" })).errorCode, "WEB_LOGIN_REQUIRED");
});

test("a web inventory never carries spend authority, even with a quote and a usable offer", () => {
  const value = webInventory();
  assert.equal(value.quoteId, null);
  assert.equal(value.credits[0].remainingCount, 2, "true quantities are kept");
  assert.equal(resetUseBlock(value, value.credits[0], 1001), "web");
  assert.equal(canUseReset(value, value.credits[0], 1001), false);
});

test("web offers are current only for a fresh successful connected read, otherwise a labelled last observation", () => {
  const fresh = webInventory();
  assert.equal(resetWebOffer(fresh, fresh.credits[0], 2000), "available");
  assert.equal(resetWebOffer(fresh, credit({ usable: false }), 2000), "unavailable");
  assert.equal(resetWebOffer(fresh, credit({ expiresAt: 1500 }), 2000), "unavailable");
  assert.equal(resetWebOffer(fresh, credit({ cooldownUntil: 9000 }), 2000), "unavailable");
  // A missed five-minute refresh must not look current.
  assert.equal(resetWebStale(fresh, 1000 + 5 * 60_000), false);
  assert.equal(resetWebStale(fresh, 1000 + 5 * 60_000 + 1), true);
  assert.equal(resetWebOffer(fresh, fresh.credits[0], 1000 + 5 * 60_000 + 1), "stale");
  assert.equal(resetWebStale(fresh, 999), true, "a clock earlier than the observation is not current");
  for (const web of [webStatus({ state: "login_required" }), webStatus({ state: "waiting_browser" }), webStatus({ state: "error", errorCode: "WEB_FETCH_FAILED" }), webStatus({ lastSuccessAt: null })]) {
    const value = webInventory({ web });
    assert.equal(resetWebOffer(value, value.credits[0], 2000), "stale", web.state);
  }
  assert.equal(resetWebStale(webInventory({ state: "unknown", availableCount: null, redeemableCount: null }), 2000), true);
  assert.equal(resetWebStale(parsed(), 2000), true, "an OAuth inventory is not a web observation");
});
