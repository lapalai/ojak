import { test } from "node:test";
import assert from "node:assert/strict";
import { canStartLogin, findLoginJob, isTerminalLogin, loginFailure, loginProviderOf, loginStep, matchesRequest, mergeLoginStatus, parseLoginStatus, reflectedInSnapshot, reloginRequest, startArgs } from "./login.ts";
import type { LoginStatus } from "./login.ts";
import type { Account } from "./types.ts";

const account = (over: Partial<Account>): Account => ({ id: "a", provider: "anthropic", tool: "claude", label: "L", email: "me@x.com", organization: null, plan: null, profilePath: null, binaryPath: null, identityKey: null, authStatus: "authenticated", verification: "v", canLaunch: true, reason: null, enabled: true, maxConcurrency: 1, buckets: [], lastCheckedAt: 0, ...over });
const status = (over: Partial<LoginStatus> = {}): LoginStatus => ({ id: "j1", provider: "anthropic", targetAccountId: null, accountId: null, state: "waiting", identity: null, error: null, startedAt: 1, updatedAt: 1, ...over });

test("maps accounts to official login providers and rejects unsupported ones", () => {
  assert.equal(loginProviderOf({ tool: "claude", provider: "anthropic" }), "anthropic");
  assert.equal(loginProviderOf({ tool: "codex", provider: "openai" }), "openai-codex");
  assert.equal(loginProviderOf({ tool: "omp", provider: "xai" }), "xai-oauth");
  assert.equal(loginProviderOf({ tool: "omp", provider: "google" }), "google-antigravity");
  assert.equal(loginProviderOf({ tool: "omp", provider: "other" }), null);
});

test("new Claude/Codex accounts need a name; relogin and xAI/Google do not", () => {
  assert.equal(canStartLogin({ provider: "anthropic", label: "  " }), false);
  assert.equal(canStartLogin({ provider: "openai-codex", label: "Work" }), true);
  assert.equal(canStartLogin({ provider: "anthropic", accountId: "a" }), true);
  assert.equal(canStartLogin({ provider: "xai-oauth" }), true);
});

test("start args are flat, trimmed, and carry the settings digest only for new native accounts", () => {
  assert.deepEqual(startArgs({ provider: "anthropic", label: " Work ", settingsDigest: "d1" }), { provider: "anthropic", label: "Work", settingsDigest: "d1" });
  assert.deepEqual(startArgs({ provider: "anthropic", label: "Work", settingsDigest: null }), { provider: "anthropic", label: "Work" });
  assert.deepEqual(startArgs({ provider: "anthropic", accountId: "acc-1", label: "ignored", settingsDigest: "d1", accountName: "me" }), { provider: "anthropic", accountId: "acc-1" });
  assert.deepEqual(startArgs({ provider: "xai-oauth", label: "ignored" }), { provider: "xai-oauth" });
});

test("relogin prefers the official CLI account and falls back to an expired omp account", () => {
  const cli = account({ id: "c", canLaunch: false, authStatus: "auth-required" });
  const omp = account({ id: "o", tool: "omp", canLaunch: false, authStatus: "auth-required" });
  assert.equal(reloginRequest([omp, cli], () => true)?.accountId, "c");
  assert.equal(reloginRequest([omp], () => true)?.accountId, "o");
  assert.equal(reloginRequest([omp], tool => tool !== "claude"), null, "omp Claude needs the Claude CLI installed");
  const grok = account({ id: "g", tool: "omp", provider: "xai", canLaunch: false, authStatus: "auth-required" });
  assert.deepEqual(reloginRequest([grok], () => false), { provider: "xai-oauth", accountId: "g", accountName: "me@x.com", workspace: null });
  assert.equal(reloginRequest([account({ id: "z", tool: "omp", provider: "other", authStatus: "auth-required" })], () => true), null);
});

test("matchesRequest compares only provider and target account, never state", () => {
  assert.equal(matchesRequest(status({ targetAccountId: "a" }), { provider: "anthropic", accountId: "a" }), true);
  assert.equal(matchesRequest(status({ targetAccountId: "b" }), { provider: "anthropic", accountId: "a" }), false);
  assert.equal(matchesRequest(status({ targetAccountId: null }), { provider: "anthropic" }), true);
  assert.equal(matchesRequest(status({ targetAccountId: null }), { provider: "anthropic", accountId: "a" }), false);
  assert.equal(matchesRequest(status({ targetAccountId: "a" }), { provider: "anthropic" }), false);
  assert.equal(matchesRequest(status({ provider: "xai-oauth" }), { provider: "anthropic" }), false);
  for (const state of ["waiting", "succeeded", "failed", "canceled"] as const) {
    assert.equal(matchesRequest(status({ state }), { provider: "anthropic" }), true, state);
  }
});

const NOW = 10_000_000;
const none: ReadonlySet<string> = new Set();
const done = (over: Partial<LoginStatus> = {}) => status({ state: "succeeded", accountId: "a", startedAt: NOW - 5000, updatedAt: NOW - 1000, ...over });
const open = (list: LoginStatus[], request: Parameters<typeof findLoginJob>[1], acknowledged: ReadonlySet<string> = none) => findLoginJob(list, request, acknowledged);

test("close → complete → reopen shows the finished result, whatever the outcome", () => {
  const request = { provider: "anthropic", accountId: "a" } as const;
  const ok = done({ id: "ok", targetAccountId: "a" });
  const failed = status({ id: "bad", targetAccountId: "a", state: "failed", startedAt: NOW - 5000, updatedAt: NOW - 1000, error: { code: "LOGIN_FAILED", message: "x", retryable: true } });
  const canceled = status({ id: "no", targetAccountId: "a", state: "canceled", startedAt: NOW - 5000, updatedAt: NOW - 1000 });
  assert.equal(open([ok], request), ok);
  assert.equal(open([failed], request), failed);
  assert.equal(open([canceled], request), canceled);
  // 새 계정 로그인(대상 없음)도 같다.
  const added = done({ id: "added", targetAccountId: null, accountId: "new" });
  assert.equal(open([added], { provider: "anthropic" }), added);
});

test("a running job wins over any finished one; the latest job wins among the same kind", () => {
  const request = { provider: "openai-codex" } as const;
  const oldOk = done({ id: "old", provider: "openai-codex", startedAt: 1, updatedAt: NOW - 3000 });
  const running = status({ id: "run", provider: "openai-codex", state: "verifying", startedAt: NOW - 500, updatedAt: NOW - 100 });
  assert.equal(open([oldOk, running], request), running);
  assert.equal(open([running, oldOk], request), running);
  // 같은 대상의 끝난 작업이 여럿이면 가장 최근에 시작한 작업의 결과다. 오래된 성공이 새 실패를, 오래된 실패가 새 성공을 가리지 않는다.
  const olderOk = done({ id: "ok", provider: "openai-codex", startedAt: NOW - 9000, updatedAt: NOW - 8000 });
  const newerFail = status({ id: "fail", provider: "openai-codex", state: "failed", startedAt: NOW - 4000, updatedAt: NOW - 3500, error: { code: "LOGIN_FAILED", message: "x", retryable: true } });
  assert.equal(open([olderOk, newerFail], request), newerFail);
  assert.equal(open([newerFail, olderOk], request), newerFail);
  const olderFail = status({ id: "fail2", provider: "openai-codex", state: "failed", startedAt: NOW - 9000, updatedAt: NOW - 8000 });
  const newerOk = done({ id: "ok2", provider: "openai-codex", startedAt: NOW - 4000, updatedAt: NOW - 3500 });
  assert.equal(open([newerOk, olderFail], request), newerOk);
  // 시작 시각이 같으면 나중에 갱신된 쪽.
  const tieA = done({ id: "tieA", startedAt: 5, updatedAt: NOW - 2000 });
  const tieB = status({ id: "tieB", state: "canceled", startedAt: 5, updatedAt: NOW - 1000 });
  assert.equal(open([tieA, tieB], { provider: "anthropic" }), tieB);
});

test("a seen latest result never hides the start screen, and never resurrects an older unseen one", () => {
  const request = { provider: "anthropic" } as const;
  const oldNewAccount = done({ id: "enrolled", accountId: "first" });
  // 이미 결과를 본 작업은 다시 열어도 새 로그인 화면이다(명시적 재시작·새 계정 추가).
  assert.equal(open([oldNewAccount], request, new Set(["enrolled"])), null);
  // 오래돼도 못 봤으면 앱이 살아 있는 동안 복원한다(시간 제한 없음).
  assert.equal(open([done({ id: "ancient", startedAt: 1, updatedAt: 2 })], request)?.id, "ancient");
  // 최신 작업을 이미 봤으면 더 오래된 못 본 결과가 다시 나타나지 않는다. 순서와 무관하다.
  const seenNewer = done({ id: "seen", startedAt: NOW - 1000, updatedAt: NOW - 900 });
  const unseenOlder = status({ id: "unseen", state: "failed", startedAt: NOW - 8000, updatedAt: NOW - 7000 });
  assert.equal(open([seenNewer, unseenOlder], request, new Set(["seen"])), null);
  assert.equal(open([unseenOlder, seenNewer], request, new Set(["seen"])), null);
  // 최신이 못 본 결과면 오래된 본 결과가 있어도 최신이 보인다.
  const seenOlder = done({ id: "seenOld", startedAt: NOW - 8000, updatedAt: NOW - 7000 });
  const unseenNewer = status({ id: "unseenNew", state: "canceled", startedAt: NOW - 1000, updatedAt: NOW - 900 });
  assert.equal(open([seenOlder, unseenNewer], request, new Set(["seenOld"]))?.id, "unseenNew");
  // 진행 중인 작업은 본 적이 있어도(목록에 있는 한) 이어 붙는다.
  const running = status({ id: "run", state: "waiting", startedAt: NOW - 100, updatedAt: NOW - 50 });
  assert.equal(open([running], request, new Set(["run"])), running);
});

test("provider and target account are isolated when reopening", () => {
  const a = done({ id: "A", targetAccountId: "a" });
  const b = status({ id: "B", targetAccountId: "b", state: "failed", startedAt: NOW - 100, updatedAt: NOW - 50 });
  const grok = done({ id: "G", provider: "xai-oauth", targetAccountId: null, accountId: "g" });
  const fresh = done({ id: "N", targetAccountId: null, accountId: "n" });
  const list = [a, b, grok, fresh];
  assert.equal(open(list, { provider: "anthropic", accountId: "a" }), a);
  assert.equal(open(list, { provider: "anthropic", accountId: "b" }), b);
  assert.equal(open(list, { provider: "anthropic", accountId: "zzz" }), null);
  assert.equal(open(list, { provider: "anthropic" }), fresh);
  assert.equal(open(list, { provider: "xai-oauth" }), grok);
  assert.equal(open(list, { provider: "openai-codex" }), null);
  // 같은 공급자의 다른 계정 작업이 진행 중이어도 이 요청의 결과 화면을 바꾸지 않는다(시작은 네이티브가 LOGIN_BUSY로 막는다).
  const otherRunning = status({ id: "R", targetAccountId: "b", state: "waiting", startedAt: NOW - 10, updatedAt: NOW - 5 });
  assert.equal(open([a, otherRunning], { provider: "anthropic", accountId: "a" }), a);
  assert.equal(open([], { provider: "anthropic" }), null);
});

test("parsing rejects bad shapes and never reports success without a verified account", () => {
  assert.equal(parseLoginStatus(null), null);
  assert.equal(parseLoginStatus({ id: "j", provider: "zai", state: "waiting", targetAccountId: null }), null);
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "done", targetAccountId: null }), null);
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "waiting" }), null, "targetAccountId is required");
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "waiting", targetAccountId: "" }), null);
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "waiting", targetAccountId: 7 }), null);
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "waiting", targetAccountId: "a" })?.targetAccountId, "a");
  const unverified = parseLoginStatus({ id: "j", provider: "anthropic", state: "succeeded", targetAccountId: null, accountId: null });
  assert.equal(unverified?.state, "failed");
  assert.equal(unverified?.error?.code, "LOGIN_UNVERIFIED");
  const ok = parseLoginStatus({ id: "j", provider: "anthropic", state: "succeeded", targetAccountId: null, accountId: "a", identity: { label: "m***@x.com", workspace: "Acme" }, error: { code: "X" } });
  assert.equal(ok?.state, "succeeded");
  assert.equal(ok?.error, null);
  assert.deepEqual(ok?.identity, { label: "m***@x.com", workspace: "Acme" });
});

test("a failed job without a reason still shows a failure, and secrets or URLs are stripped from messages", () => {
  assert.equal(parseLoginStatus({ id: "j", provider: "anthropic", state: "failed", targetAccountId: null })?.error?.code, "LOGIN_FAILED");
  const error = parseLoginStatus({ id: "j", provider: "anthropic", state: "failed", targetAccountId: null, error: { code: "LOGIN_FAILED", message: "open https://auth.example/cb?code=abc&state=zz failed, access_token=SECRET", retryable: true } })?.error;
  assert.ok(error && !error.message.includes("auth.example") && !error.message.includes("SECRET"));
});

test("polling merges forward only: terminal jobs never revive, older and foreign responses are dropped", () => {
  const waiting = status({ updatedAt: 5 });
  const verifying = status({ state: "verifying", updatedAt: 6 });
  assert.equal(mergeLoginStatus(null, waiting), waiting);
  assert.equal(mergeLoginStatus(waiting, verifying), verifying);
  assert.equal(mergeLoginStatus(verifying, waiting), verifying);
  const done = status({ state: "canceled", updatedAt: 7 });
  assert.equal(mergeLoginStatus(done, status({ state: "waiting", updatedAt: 9 })), done);
  assert.equal(mergeLoginStatus(waiting, status({ id: "other", state: "succeeded", accountId: "a", updatedAt: 99 })), waiting);
});

test("steps and terminal states", () => {
  assert.deepEqual((["starting", "waiting", "verifying", "syncing", "succeeded", "failed", "canceled"] as const).map(loginStep), [0, 0, 1, 2, 3, null, null]);
  assert.deepEqual((["waiting", "succeeded", "failed", "canceled"] as const).map(isTerminalLogin), [false, true, true, true]);
});

test("success counts as reflected only for a verified, signed-in native account or a non-stale omp account", () => {
  const native = (over: Partial<Account> = {}) => account({ id: "a", tool: "claude", authStatus: "authenticated", canLaunch: true, verification: "preflight-verified", ...over });
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native()]), true);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ verification: "runtime-confirmed" })]), true);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ authStatus: "auth-required", canLaunch: false })]), false, "stale old account");
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ authStatus: "error" })]), false);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ canLaunch: false })]), false);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ verification: "observed" })]), false);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ verification: "unverified" })]), false);
  assert.equal(reflectedInSnapshot({ accountId: "a" }, [native({ id: "b" })]), false);
  assert.equal(reflectedInSnapshot({ accountId: null }, [native()]), false);
  // omp 계정은 백엔드가 공식 OAuth·읽기 전용 사용량을 확인한 뒤 성공을 보고한다. 관측 상태여도 되지만 오래된 로그인 필요·오류는 아니다.
  const omp = (over: Partial<Account> = {}) => account({ id: "o", tool: "omp", canLaunch: false, verification: "observed", authStatus: "authenticated", ...over });
  assert.equal(reflectedInSnapshot({ accountId: "o" }, [omp()]), true);
  assert.equal(reflectedInSnapshot({ accountId: "o" }, [omp({ authStatus: "auth-required" })]), false);
  assert.equal(reflectedInSnapshot({ accountId: "o" }, [omp({ authStatus: "error" })]), false);
});

test("login failures use the login-specific mismatch text", () => {
  assert.equal(loginFailure({ code: "ACCOUNT_MISMATCH", message: "m", retryable: true }).code, "LOGIN_ACCOUNT_MISMATCH");
  assert.equal(loginFailure({ code: "LOGIN_TIMEOUT", message: "", retryable: true }).code, "LOGIN_TIMEOUT");
  assert.equal(loginFailure({ code: "SOMETHING_NEW", message: "", retryable: true }).code, "LOGIN_FAILED");
  assert.equal(loginFailure({ code: "SOMETHING_NEW", message: "raw", retryable: true }).code, "SOMETHING_NEW");
});
