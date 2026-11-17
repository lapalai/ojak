import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../src-tauri/src/reset_web_collect.js", import.meta.url), "utf8");
const org = "04390c26-0cba-4b90-918f-8a0804c80157";
const account = (uuid = "user-a", organization = org) => ({ uuid, email_address: `${uuid}@example.test`, memberships: [{ organization: { uuid: organization } }], full_name: "DO_NOT_EXPORT" });
const usage = { cedar_ember: { eligible: true, grants: [{ id: "weekly", type: "weekly", resets_left: 1, starts_at: null, ends_at: null, clears: ["seven_day"], blocking: [], usable_now: true, notes: "DO_NOT_EXPORT" }], event_props: { secret: "DO_NOT_EXPORT" } }, conversations: "DO_NOT_EXPORT" };
const json = (body: unknown) => new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
async function collect(fetcher: (path: string, init: RequestInit) => Promise<Response>, origin = "https://claude.ai") {
  const context: Record<string, unknown> = { location: { origin }, fetch: fetcher, AbortController, TextDecoder, setTimeout, clearTimeout };
  runInNewContext(`(${source})(${JSON.stringify(org)}, "resultSlot")`, context);
  const state = context.resultSlot as { done: boolean; result: Record<string, unknown> };
  // Wait only for finite local promise work; a stuck fixture is a failure, never a real 12s network timeout.
  for (let turn = 0; turn < 100 && !state.done; turn++) await new Promise<void>(resolve => setImmediate(resolve));
  assert.equal(state.done, true);
  return JSON.parse(JSON.stringify(state.result)) as Record<string, unknown>;
}

test("web collection rejects a session switched between account verification and usage completion", async () => {
  let reads = 0;
  const result = await collect(async path => json(path === "/api/account" ? account(++reads === 1 ? "user-a" : "user-b") : usage));
  assert.deepEqual(result, { errorCode: "WEB_ACCOUNT_MISMATCH" });
});

test("an unrelated organization or origin cannot trigger usage reads", async () => {
  let reads = 0;
  const result = await collect(async path => {
    reads++;
    assert.equal(path, "/api/account", "membership must be verified before requesting an organization's usage");
    return json(account("user-a", "other-org"));
  });
  assert.deepEqual(result, { errorCode: "WEB_ACCOUNT_MISMATCH" });
  assert.equal(reads, 1);
  const foreign = await collect(async () => { throw new Error("foreign origin must never fetch"); }, "https://claude.ai.example.test");
  assert.deepEqual(foreign, { errorCode: "WEB_ACCOUNT_MISMATCH" });
});

test("successful web reads export only binding and reset information, not account or provider metadata", async () => {
  const result = await collect(async path => json(path === "/api/account" ? account() : usage));
  assert.equal(result.accountUuid, "user-a");
  assert.equal(result.member, true);
  assert.equal(JSON.stringify(result).includes("DO_NOT_EXPORT"), false);
  const values = result.values as Array<{ cedar_ember: { grants: Array<{ id: string; resets_left: number }> } }>;
  assert.deepEqual(values.map(value => value.cedar_ember.grants.map(grant => [grant.id, grant.resets_left])), [[["weekly", 1]], [["weekly", 1]]]);
});

test("expired web login and malformed provider responses cannot become an empty successful inventory", async () => {
  assert.deepEqual(await collect(async () => new Response(null, { status: 401 })), { errorCode: "WEB_LOGIN_REQUIRED" });
  assert.deepEqual(await collect(async path => path === "/api/account" ? json(account()) : json({ cedar_ember: { eligible: true, grants: "not-an-array" } })), { errorCode: "WEB_RESPONSE_INVALID" });
});
