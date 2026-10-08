import { test } from "node:test";
import assert from "node:assert/strict";
import { nextFirstSuccessStage, onboardingProgress, onboardingSteps, parseFirstSuccessStage, preferredTool, serviceReady, shouldShowFirstSuccess, terminalReady } from "./onboarding.ts";
import type { FirstSuccessStage, OnboardingStep } from "./onboarding.ts";

const base = { service: true, terminal: true, accountCount: 2, sessionCount: 1, tools: ["claude", "codex"], installed: ["claude", "codex"] };
const byId = (steps: OnboardingStep[]) => Object.fromEntries(steps.map(step => [step.id, step]));

test("steps keep the screen order and report done/todo", () => {
  const steps = onboardingSteps(base);
  assert.deepEqual(steps.map(step => [step.id, step.state]), [["service", "done"], ["terminal", "done"], ["account", "done"], ["first", "done"]]);
  assert.equal(byId(steps).account.count, 2);
});

test("with no account the account step offers login for installed tools and nothing else dead-ends", () => {
  const steps = byId(onboardingSteps({ service: true, terminal: false, accountCount: 0, sessionCount: 0, tools: [], installed: ["codex", "claude", "omp"] }));
  assert.deepEqual(steps.account.action, { kind: "login", tools: ["claude", "codex"] });
  // 터미널·첫 실행은 계정이 생기기 전에는 누를 동작이 없다.
  assert.equal(steps.terminal.action, null);
  assert.equal(steps.first.action, null);
  assert.equal(steps.first.state, "todo");
  // 설치된 도구가 없으면 로그인 버튼 목록이 비어 화면이 설치 안내를 보인다.
  assert.deepEqual(byId(onboardingSteps({ ...base, accountCount: 0, sessionCount: 0, tools: [], installed: [] })).account.action, { kind: "login", tools: [] });
});

test("accounts but no session: the first-run step opens a terminal for the preferred tool", () => {
  const steps = byId(onboardingSteps({ ...base, sessionCount: 0 }));
  assert.equal(steps.account.state, "done");
  assert.equal(steps.account.action, null);
  assert.deepEqual(steps.first.action, { kind: "terminal", tool: "claude" });
  assert.deepEqual(byId(onboardingSteps({ ...base, sessionCount: 0, tools: ["codex"] })).first.action, { kind: "terminal", tool: "codex" });
});

test("the first-run step waits for a working service and terminal connection", () => {
  assert.equal(byId(onboardingSteps({ ...base, terminal: false, sessionCount: 0 })).first.action, null);
  assert.equal(byId(onboardingSteps({ ...base, service: false, sessionCount: 0 })).first.action, null);
});

test("a todo terminal or service step with something to fix points at the setup guide", () => {
  const steps = byId(onboardingSteps({ ...base, service: false, terminal: false, sessionCount: 0 }));
  assert.deepEqual(steps.service.action, { kind: "setup" });
  assert.deepEqual(steps.terminal.action, { kind: "setup" });
});

test("all four done is complete, and progress counts done steps", () => {
  assert.deepEqual(onboardingProgress(onboardingSteps(base)), { done: 4, total: 4, complete: true });
  assert.deepEqual(onboardingProgress(onboardingSteps({ ...base, sessionCount: 0 })), { done: 3, total: 4, complete: false });
  assert.deepEqual(onboardingProgress(onboardingSteps({ service: true, terminal: false, accountCount: 0, sessionCount: 0, tools: [], installed: [] })), { done: 1, total: 4, complete: false });
});

test("preferredTool prefers claude, then codex, and ignores accounts that can't launch", () => {
  const account = (tool: string, canLaunch = true, enabled = true) => ({ tool, canLaunch, enabled });
  assert.equal(preferredTool([account("codex"), account("claude")]), "claude");
  assert.equal(preferredTool([account("codex"), account("claude", false)]), "codex");
  assert.equal(preferredTool([account("claude", true, false), account("codex")]), "codex");
  assert.equal(preferredTool([account("omp")]), null);
  assert.equal(preferredTool([]), null);
});

test("service and terminal readiness use the same rules as the setup guide", () => {
  const tool = (over: { account?: boolean; connected?: boolean; verified?: boolean | null } = {}) => ({ account: true, connected: true, verified: null, ...over });
  assert.equal(terminalReady({ shell: true, tools: [tool()] }), true);
  assert.equal(terminalReady({ shell: true, tools: [] }), false);
  assert.equal(terminalReady({ shell: true, tools: [tool({ account: false, connected: false })] }), false);
  assert.equal(terminalReady({ shell: true, tools: [tool({ account: false, connected: false }), tool()] }), true);
  assert.equal(terminalReady({ shell: false, tools: [tool()] }), false);
  assert.equal(terminalReady({ shell: true, tools: [tool({ verified: false })] }), false);
  assert.equal(terminalReady({ shell: true, tools: [tool()], notices: [{ step: "terminal" }] }), false);
  assert.equal(serviceReady({ service: true }), true);
  assert.equal(serviceReady({ service: true, notices: [{ step: "service" }] }), false);
  assert.equal(serviceReady({ service: false }), false);
});

test("the first-success notice appears once, only for a session that starts after we saw none", () => {
  let stage: FirstSuccessStage | null = null;
  stage = nextFirstSuccessStage(stage, 0);
  assert.equal(stage, "watching");
  assert.equal(shouldShowFirstSuccess(stage, 0), false);
  assert.equal(shouldShowFirstSuccess(stage, 1), true);
  stage = nextFirstSuccessStage(stage, 1);
  assert.equal(stage, "show");
  // 닫기 전에는 앱을 다시 켜도 계속 보인다.
  assert.equal(shouldShowFirstSuccess(stage, 3), true);
  assert.equal(nextFirstSuccessStage(stage, 3), "show");
  // 닫으면 다시는 안 보인다.
  assert.equal(shouldShowFirstSuccess("done", 1), false);
  assert.equal(nextFirstSuccessStage("done", 0), "done");
});

test("someone who already had sessions when first seen is not told 'just started'", () => {
  assert.equal(shouldShowFirstSuccess(null, 5), false);
  assert.equal(nextFirstSuccessStage(null, 5), "done");
});

test("stored stage values are validated", () => {
  assert.equal(parseFirstSuccessStage("show"), "show");
  assert.equal(parseFirstSuccessStage("1"), null);
  assert.equal(parseFirstSuccessStage(null), null);
});
