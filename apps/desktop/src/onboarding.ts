/// 첫 실행 안내의 순수 판정. 화면(`Onboarding.tsx`)과 준비 창(`SetupGuide.tsx`)이 같은 기준을 쓰도록 여기에 둔다.
/// React·저장소·번역에 기대지 않아 `node --test`로 바로 검증한다.

/// Ojak이 계정을 골라 주는 공식 CLI. 앞에 있을수록 먼저 권한다.
export const LAUNCH_TOOLS = ["claude", "codex"] as const;
export type LaunchTool = typeof LAUNCH_TOOLS[number];

export type StepId = "service" | "terminal" | "account" | "first";
export type StepAction =
  /// 준비 창을 연다(서비스·명령 연결 설치는 거기서 한 번의 동의로 한다).
  | { kind: "setup" }
  /// 계정이 하나도 없을 때 바로 쓰는 로그인 버튼. 설치된 도구만 담는다(비면 설치 안내).
  | { kind: "login"; tools: LaunchTool[] }
  /// 명령이 입력된 터미널을 연다.
  | { kind: "terminal"; tool: LaunchTool };

export interface OnboardingStep {
  id: StepId;
  state: "done" | "todo";
  /// 계정 단계의 연결 수. 다른 단계에서는 0.
  count: number;
  /// 할 일이 남았고 사용자가 지금 누를 수 있는 동작이 있을 때만 값이 있다.
  action: StepAction | null;
}

export interface OnboardingInput {
  service: boolean;
  terminal: boolean;
  accountCount: number;
  /// Ojak이 관리한 세션 수(`snapshot.sessions.length`).
  sessionCount: number;
  /// 지금 실행할 수 있는 계정이 있는 도구 id.
  tools: readonly string[];
  /// 설치된 도구 id(로그인 버튼용).
  installed: readonly string[];
}

/// 도구 목록에서 먼저 권할 도구. claude가 있으면 claude, 아니면 codex, 없으면 null.
export function pickTool(tools: readonly string[]): LaunchTool | null {
  return LAUNCH_TOOLS.find(tool => tools.includes(tool)) ?? null;
}

/// 지금 실행할 수 있는 Claude·Codex 계정만 남긴다.
export function launchableAccounts<T extends { tool: string; enabled: boolean; canLaunch: boolean }>(accounts: readonly T[]): T[] {
  return accounts.filter(account => (LAUNCH_TOOLS as readonly string[]).includes(account.tool) && account.enabled && account.canLaunch);
}

/// 터미널을 열어 줄 도구. 실행할 수 있는 계정이 있는 도구 중 claude 먼저.
export function preferredTool(accounts: readonly { tool: string; enabled: boolean; canLaunch: boolean }[]): LaunchTool | null {
  return pickTool(launchableAccounts(accounts).map(account => account.tool));
}

/// 준비 단계 네 개. 순서가 곧 화면 순서다.
export function onboardingSteps(input: OnboardingInput): OnboardingStep[] {
  const launch = pickTool(input.tools);
  const accounts = input.accountCount > 0;
  const login = LAUNCH_TOOLS.filter(tool => input.installed.includes(tool));
  const started = input.sessionCount > 0;
  return [
    { id: "service", state: input.service ? "done" : "todo", count: 0, action: input.service ? null : { kind: "setup" } },
    // 명령 연결은 계정이 있는 도구에만 설치된다. 계정이 없으면 아래 계정 단계가 먼저다.
    { id: "terminal", state: input.terminal ? "done" : "todo", count: 0, action: input.terminal || !accounts ? null : { kind: "setup" } },
    { id: "account", state: accounts ? "done" : "todo", count: input.accountCount, action: accounts ? null : { kind: "login", tools: login } },
    // 서비스와 명령 연결이 안 됐으면 터미널을 열어도 Ojak을 거치지 않는다. 먼저 위 단계를 끝내게 한다.
    { id: "first", state: started ? "done" : "todo", count: 0, action: started || !launch || !input.service || !input.terminal ? null : { kind: "terminal", tool: launch } },
  ];
}

export function onboardingProgress(steps: readonly OnboardingStep[]): { done: number; total: number; complete: boolean } {
  const done = steps.filter(step => step.state === "done").length;
  return { done, total: steps.length, complete: done === steps.length };
}

interface SetupLike {
  service: boolean;
  shell: boolean;
  tools: { account: boolean; connected: boolean; verified: boolean | null }[];
  notices?: { step: string }[];
}

/// 준비 창의 '서비스' 단계와 같은 기준.
export function serviceReady(status: Pick<SetupLike, "service" | "notices">): boolean {
  return status.service && !(status.notices ?? []).some(notice => notice.step === "service");
}

/// 준비 창의 '터미널 연결' 단계와 같은 기준: 계정이 있고, 그 도구마다 명령이 연결돼 있으며 확인에 실패하지 않았다.
export function terminalReady(status: Pick<SetupLike, "shell" | "tools" | "notices">): boolean {
  const withAccount = status.tools.filter(tool => tool.account);
  return withAccount.length > 0 && status.shell
    && withAccount.every(tool => tool.connected && tool.verified !== false)
    && !(status.notices ?? []).some(notice => notice.step === "terminal");
}

/// 첫 세션 안내의 진행. `null`(아직 지켜보지 않음) → `watching`(세션 0개를 봄) → `show`(그 뒤 첫 세션이 생김, 안내 중) → `done`(닫음).
/// 이미 세션이 있는 상태로 처음 본 사용자는 조용히 `done`이 된다(방금 시작한 게 아니므로).
export type FirstSuccessStage = "watching" | "show" | "done";

export function nextFirstSuccessStage(stage: FirstSuccessStage | null, sessionCount: number): FirstSuccessStage {
  if (stage === "done" || stage === "show") return stage;
  if (sessionCount === 0) return "watching";
  return stage === "watching" ? "show" : "done";
}

/// 안내를 지금 보여야 하나. 닫아서 `done`이 되기 전까지(앱을 다시 켜도) 보인다.
export function shouldShowFirstSuccess(stage: FirstSuccessStage | null, sessionCount: number): boolean {
  return sessionCount > 0 && nextFirstSuccessStage(stage, sessionCount) === "show";
}

export function parseFirstSuccessStage(value: string | null): FirstSuccessStage | null {
  return value === "watching" || value === "show" || value === "done" ? value : null;
}
