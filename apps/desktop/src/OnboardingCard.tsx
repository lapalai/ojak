import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { CheckCircle2, Terminal, X } from "lucide-react";
import { ActionFeedback, Dot } from "./components";
import { openPrefilledTerminal, setupStatus } from "./api";
import type { SetupStatus } from "./api";
import { isWindows, t } from "./i18n";
import { launchableAccounts, nextFirstSuccessStage, onboardingProgress, onboardingSteps, parseFirstSuccessStage, preferredTool, serviceReady, shouldShowFirstSuccess, terminalReady } from "./onboarding";
import type { LaunchTool, OnboardingStep } from "./onboarding";
import { toolNames, useAction } from "./state";
import type { ApiError, Snapshot } from "./types";

const DISMISS_KEY = "ojak.onboardingDismissed";
const FIRST_SUCCESS_KEY = "ojak.firstSuccess";
/// 로그인 버튼에는 제품 이름만 쓴다(`Claude Code 로그인`보다 짧다).
const loginNames: Record<LaunchTool, string> = { claude: "Claude", codex: "Codex" };

function readStorage(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}
function writeStorage(key: string, value: string | null): void {
  try { if (value === null) localStorage.removeItem(key); else localStorage.setItem(key, value); } catch { /* 저장소가 막혀도 이번 창에서는 동작한다. */ }
}

/// 번역 문장 안의 `백틱` 구간을 코드 모양으로 그린다.
export function codeText(text: string): ReactNode {
  return text.split("`").map((part, index) => index % 2 ? <code key={index}>{part}</code> : part);
}

/// 입력줄에 `claude`/`codex`만 적힌 터미널을 연다. 실행은 사용자가 Enter를 칠 때뿐이다.
/// 열 수 없는 환경(Windows·zsh 아님)은 명령을 보여 주고 복사 버튼을 둔다.
export function OpenTerminal({ tool }: { tool: LaunchTool }) {
  const action = useAction();
  const copy = useAction();
  const [fallback, setFallback] = useState(isWindows);
  const [prefilled, setPrefilled] = useState(false);
  const open = () => {
    void action.run(async () => {
      const result = await openPrefilledTerminal(tool);
      setPrefilled(result.opened && result.prefilled);
      if (!result.prefilled) setFallback(true);
    });
  };
  const copyCommand = () => {
    void copy.run(async () => {
      try { await navigator.clipboard.writeText(tool); } catch { throw { code: "CLIPBOARD_FAILED", message: t("hosts.clipboardFailed"), retryable: true } satisfies ApiError; }
    }, () => t("onboarding.copied"));
  };
  return <div className="open-terminal">
    {fallback ? <div className="open-terminal-row">
      <code className="command-chip">{tool}</code>
      <button type="button" className="button" disabled={copy.pending} onClick={copyCommand}>{t("common.copy")}</button>
      <span className="field-help">{t("onboarding.typeInTerminal")}</span>
    </div> : <div className="open-terminal-row">
      <button type="button" className="button" disabled={action.pending} onClick={open}><Terminal size={13} />{t("onboarding.openTerminal")}</button>
      {prefilled && <span className="field-help" role="status">{t("onboarding.enterOnly")}</span>}
    </div>}
    <ActionFeedback error={action.error ?? copy.error} message={copy.message} />
  </div>;
}

/// 준비 상태(서비스·명령 연결). 처음, 계정이 바뀔 때, `revision`이 바뀔 때, 창으로 돌아올 때 다시 읽는다. 셸을 실행하지 않는 읽기 전용 조회다.
function useSetupStatus(accountKey: string, revision: number): { status: SetupStatus | null; failed: boolean } {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    const load = () => {
      void setupStatus().then(next => { if (active) { setStatus(next); setFailed(false); } }).catch(() => { if (active) setFailed(true); });
    };
    load();
    window.addEventListener("focus", load);
    return () => { active = false; window.removeEventListener("focus", load); };
  }, [accountKey, revision]);
  return { status, failed };
}

/// 사이드바 '시작하기'를 누를 때마다 값이 커진다. 화면이 다시 만들어져도 같은 요청으로 다시 펼치지 않게 처리한 값을 기억한다.
let handledRequest = 0;

function stepText(step: OnboardingStep): string {
  switch (step.id) {
    case "service": return t(step.state === "done" ? "onboarding.service.done" : "onboarding.service.todo");
    case "terminal": return t(step.state === "done" ? "onboarding.terminal.done" : "onboarding.terminal.todo");
    case "account": return step.state === "done" ? t("onboarding.account.done", { count: step.count }) : t("onboarding.account.todo");
    case "first": return t(step.state === "done" ? "onboarding.first.done" : "onboarding.first.todo");
  }
}

/// 사용 현황 맨 위의 '시작하기' 카드. 네 단계가 모두 끝날 때까지 남고, 끝나면 한 줄로 접힌다(닫으면 기억).
/// 준비 창의 [나중에]와 무관하다. 사이드바 '시작하기'를 누르면 닫았어도 다시 펼친다.
export function OnboardingCard({ snapshot, request, revision, onAdd, onSetup }: { snapshot: Snapshot; request: number; revision: number; onAdd: (tool: string) => void; onSetup: () => void }) {
  const accounts = launchableAccounts(snapshot.accounts);
  const accountKey = accounts.map(account => account.id).join(",");
  const { status, failed } = useSetupStatus(accountKey, revision);
  const [dismissed, setDismissed] = useState(() => readStorage(DISMISS_KEY) === "1");
  useEffect(() => {
    if (request <= handledRequest) return;
    handledRequest = request;
    writeStorage(DISMISS_KEY, null);
    setDismissed(false);
  }, [request]);
  if (!status && !failed) return null;
  // 이 화면은 서비스가 응답할 때만 보인다. 준비 상태를 못 읽어도 서비스는 켜진 것이다.
  const steps = onboardingSteps({
    service: status ? serviceReady(status) : true,
    terminal: status ? terminalReady(status) : false,
    accountCount: accounts.length,
    sessionCount: snapshot.sessions.length,
    tools: accounts.map(account => account.tool),
    installed: snapshot.tools.filter(tool => tool.installed).map(tool => tool.id),
  });
  const { done, total, complete } = onboardingProgress(steps);
  if (complete && dismissed) return null;
  if (complete) return <div className="getting-started collapsed" role="status">
    <Dot color="var(--good)" /><span>{t("onboarding.complete", { done, total })}</span>
    <button type="button" className="icon-button" aria-label={t("onboarding.dismiss")} title={t("onboarding.dismiss")} onClick={() => { writeStorage(DISMISS_KEY, "1"); setDismissed(true); }}><X size={14} /></button>
  </div>;
  const tool = preferredTool(accounts);
  return <section className="getting-started" aria-label={t("onboarding.title")}>
    <header><h2>{t("onboarding.title")}</h2><span className="aside">{done}/{total}</span></header>
    <ol className="gs-steps">{steps.map(step => <li key={step.id} className={step.state}>
      <Dot color={step.state === "done" ? "var(--good)" : "var(--tertiary)"} />
      <span className="gs-text">{stepText(step)}</span>
      <div className="gs-actions">
        {step.action?.kind === "setup" && <button type="button" className="button" onClick={onSetup}>{t("onboarding.setup")}</button>}
        {step.action?.kind === "login" && step.action.tools.map(login => <button type="button" className="button" key={login} onClick={() => onAdd(login)}>{t("setup.login", { tool: loginNames[login] })}</button>)}
      </div>
      {step.action?.kind === "login" && step.action.tools.length === 0 && <p className="field-help gs-sub">{t("setup.noTool")}</p>}
      {step.id === "first" && step.state === "todo" && (step.action?.kind === "terminal" && tool ? <div className="gs-sub"><OpenTerminal tool={tool} /></div> : <p className="field-help gs-sub">{t("onboarding.first.waiting")}</p>)}
    </li>)}</ol>
  </section>;
}

/// 처음 관리 세션이 생긴 순간 한 번만 보이는 안내. 세션이 없는 것을 본 뒤에 처음 생긴 세션에만 뜨고, 닫으면 다시 뜨지 않는다.
export function FirstSuccessNotice({ snapshot, masked }: { snapshot: Snapshot; masked: boolean }) {
  const sessionCount = snapshot.sessions.length;
  const [stage, setStage] = useState(() => parseFirstSuccessStage(readStorage(FIRST_SUCCESS_KEY)));
  useEffect(() => {
    const next = nextFirstSuccessStage(stage, sessionCount);
    if (next === stage) return;
    writeStorage(FIRST_SUCCESS_KEY, next);
    setStage(next);
  }, [stage, sessionCount]);
  if (!shouldShowFirstSuccess(stage, sessionCount)) return null;
  const first = snapshot.sessions.reduce((earliest, session) => session.startedAt < earliest.startedAt ? session : earliest);
  const account = snapshot.accounts.find(item => item.id === first.accountId);
  const label = masked ? t("privacy.accountHidden") : (account ? account.email ?? account.label : toolNames[first.tool] ?? first.tool).replaceAll("`", "'");
  return <div className="launch-notice" role="status">
    <CheckCircle2 size={16} /><span>{codeText(t("onboarding.firstSuccess", { account: label }))}</span>
    <button type="button" className="icon-button" aria-label={t("common.close")} title={t("common.close")} onClick={() => { writeStorage(FIRST_SUCCESS_KEY, "done"); setStage("done"); }}><X size={14} /></button>
  </div>;
}
