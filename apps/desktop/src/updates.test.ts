import { test } from "node:test";
import assert from "node:assert/strict";
import { updateBadgeVisible, updateControlsVisible, updateInterruptCounts, updatesEnabled } from "./updates.ts";


test("a configured pubkey shows controls, and the badge only when a version is waiting", () => {
  assert.equal(updatesEnabled("RWTt...real-minisign-pubkey"), true);
  assert.equal(updateControlsVisible({ enabled: true }), true);
  assert.equal(updateBadgeVisible({ enabled: true, available: null }), false);
  assert.equal(updateBadgeVisible({ enabled: true, available: { version: "0.2.0" } }), true);
  assert.equal(updateControlsVisible(null), false);
  assert.equal(updatesEnabled("  "), false);
  assert.equal(updatesEnabled("REPLACE_WITH_TAURI_UPDATER_PUBKEY"), false);
});

test("install warns only for live managed sessions and bridge sessions used in the last 15 minutes", () => {
  const now = 1_000_000_000_000;
  const quiet = updateInterruptCounts(
    [{ state: "EXITED" }, { state: "FAILED" }],
    [{ lastUsedAt: now - 16 * 60_000 }],
    now,
  );
  assert.deepEqual(quiet, { managed: 0, bridge: 0, warn: false });
  const busy = updateInterruptCounts(
    [{ state: "ACTIVE" }, { state: "ORPHANED" }, { state: "EXITED" }],
    [{ lastUsedAt: now - 14 * 60_000 }, { lastUsedAt: now - 20 * 60_000 }],
    now,
  );
  assert.deepEqual(busy, { managed: 2, bridge: 1, warn: true });
});
