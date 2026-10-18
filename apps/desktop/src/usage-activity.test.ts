import { test } from "node:test";
import assert from "node:assert/strict";
import { activeAccountWork } from "./usage-activity.ts";
import type { Account, ObservedSession, ProcessIdentity, Session } from "./types.ts";

const process: ProcessIdentity = { pid: 100, startedAt: "1000", bootId: "boot-a" };
const account = (id: string): Account => ({ id, provider: "anthropic", tool: "claude", label: id, email: null, organization: null, plan: null, profilePath: null, binaryPath: null, identityKey: null,
  authStatus: "authenticated", verification: "verified", canLaunch: true, reason: null, enabled: true, maxConcurrency: 1, buckets: [], lastCheckedAt: 0 });
const managed = (patch: Partial<Session> = {}): Session => ({ id: "managed", requestId: "request", accountId: "a", tool: "claude", model: "requested-model", cwd: "/work/project", state: "ACTIVE", verification: "preflight-verified", startedAt: 1, updatedAt: 2,
  process, supervisor: null, spawnAttemptId: null, generation: "generation", exitCode: null, reason: null, nativeSessionId: "thread", ...patch });
const observed = (patch: Partial<ObservedSession> = {}): ObservedSession => ({ id: "observed", tool: "claude", process, parentProcess: null, host: "terminal", cwd: "/work/project", model: "observed-model", accountId: "a", verification: "runtime-confirmed", reason: null, nativeSessionId: "thread", ...patch });

test("only active managed sessions count; reservations, uncertain leases and other accounts do not", () => {
  const rows = [managed(), ...["PREPARED", "STARTING", "SUSPECT", "ORPHANED", "FINISHED"].map(state => managed({ id: state, state })), managed({ id: "other", accountId: "b" })];
  const work = activeAccountWork([account("a")], rows, []);
  assert.deepEqual(work.map(item => item.id), ["managed"]);
  assert.equal(work[0].modelSource, "request");
});

test("profile configuration and unknown account identity never imply a running account", () => {
  const rows = [observed({ id: "configured", verification: "configured" }), observed({ id: "unverified", verification: "unverified" }), observed({ id: "identity-missing", accountId: null }), observed({ id: "other", accountId: "b" })];
  assert.deepEqual(activeAccountWork([account("a")], [], rows), []);
  const confirmed = observed({ verification: "upstream-confirmed" });
  assert.deepEqual(activeAccountWork([account("a")], [], [confirmed]).map(item => [item.id, item.modelSource]), [["observed", "observation"]]);
});

test("managed and observed records of the same process count once and show the observed model", () => {
  const work = activeAccountWork([account("a")], [managed()], [observed(), observed({ id: "duplicate" })]);
  assert.deepEqual(work.map(item => [item.id, item.model, item.modelSource]), [["observed", "observed-model", "observation"]]);
});

test("PID reuse and shared native thread IDs do not merge different live processes", () => {
  const newer = observed({ process: { ...process, startedAt: "2000" } });
  assert.deepEqual(activeAccountWork([account("a")], [managed()], [newer]).map(item => item.id), ["observed", "managed"]);
  const rebooted = observed({ process: { ...process, bootId: "boot-b" } });
  assert.equal(activeAccountWork([account("a")], [managed()], [rebooted]).length, 2);
});

test("runtime-confirmed identity takes precedence over the account chosen at launch", () => {
  const runtime = observed({ accountId: "b" });
  assert.deepEqual(activeAccountWork([account("a")], [managed()], [runtime]), []);
  assert.deepEqual(activeAccountWork([account("b")], [managed()], [runtime]).map(item => item.id), ["observed"]);
});

test("legacy records without a process identity can be reconciled by tool and native thread ID", () => {
  assert.deepEqual(activeAccountWork([account("a")], [managed({ process: null })], [observed()]).map(item => item.id), ["observed"]);
  assert.deepEqual(activeAccountWork([account("a")], [managed({ process: null })], [observed({ tool: "codex" })]).map(item => item.id), ["observed", "managed"]);
});
