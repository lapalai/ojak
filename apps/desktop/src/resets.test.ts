import { test } from "node:test";
import assert from "node:assert/strict";
import { canUseReset, parseResetInventory, parseResetOperation } from "./resets.ts";
import type { ResetInventory, ResetOperation } from "./resets.ts";

const operation = (state: ResetOperation["state"]): ResetOperation => ({ requestId: "request-a", accountId: "account-a", creditId: "credit-a", state, code: "result_unknown", startedAt: 1000, updatedAt: 1000 });
const inventory = (): ResetInventory => ({ accountId: "account-a", state: "known", availableCount: 2, redeemableCount: 2,
  observedAt: 1000, quoteId: "quote-a", operation: null, credits: [{ id: "credit-a", program: "cedar_ember", remainingCount: 2,
    usable: true, expiresAt: 200000, startsAt: null, clears: ["anthropic:5h"], requiresLimit: true, status: "available" }] });

test("unknown and unsupported inventory remain distinct from a confirmed zero balance", () => {
  const zero = { ...inventory(), availableCount: 0, redeemableCount: 0, credits: [] };
  assert.equal(parseResetInventory(zero).availableCount, 0);
  const unknown = { ...zero, state: "unknown", availableCount: null, redeemableCount: null, quoteId: null };
  assert.equal(parseResetInventory(unknown).availableCount, null);
  assert.equal(parseResetInventory({ ...unknown, state: "unsupported" }).state, "unsupported");
  assert.throws(() => parseResetInventory({ ...zero, availableCount: null }));
  assert.throws(() => parseResetInventory({ ...zero, redeemableCount: -1 }));
  assert.throws(() => parseResetOperation({ ...operation("unknown"), state: "retry" }));
});

test("confirmation expires at the quote and provider boundaries and cannot use a foreign offer", () => {
  const value = parseResetInventory(inventory()); const credit = value.credits[0];
  assert.equal(canUseReset(value, credit, 121000), true);
  assert.equal(canUseReset(value, credit, 121001), false);
  assert.equal(canUseReset(value, credit, 999), false);
  credit.expiresAt = 1001;
  assert.equal(canUseReset(value, credit, 1001), false);
  credit.expiresAt = null; credit.startsAt = 1002;
  assert.equal(canUseReset(value, credit, 1001), false);
  credit.startsAt = null;
  assert.equal(canUseReset(value, { ...credit }, 1001), false);
  assert.equal(canUseReset({ ...value, quoteId: null }, credit, 1001), false);
});

test("unconfirmed mutation outcomes prevent any further use, including a remaining multi-use grant", () => {
  const value = inventory();
  for (const state of ["preparing", "submitted", "unknown"] as const) {
    assert.equal(canUseReset({ ...value, operation: operation(state) }, value.credits[0], 1001), false);
  }
  assert.equal(canUseReset({ ...value, state: "unknown" }, value.credits[0], 1001), false);
  value.credits[0].usable = false;
  assert.equal(canUseReset(value, value.credits[0], 1001), false);
});
