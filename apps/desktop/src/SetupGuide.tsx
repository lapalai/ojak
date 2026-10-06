import { useEffect, useRef, useState } from "react";
import { CheckCircle2, Circle, Loader2 } from "lucide-react";
import { setupCheck, setupInstall, setupStatus, toApiError } from "./api";
import type { SetupStatus } from "./api";
import { ActionFeedback, Modal } from "./components";
import { useAction } from "./state";
import type { ApiError, Snapshot } from "./types";
import { setupNotice, t } from "./i18n";

const SKIP_KEY = "ojak.setupSkipped";

/// 첫 실행 준비 화면. 서비스·명령 연결·터미널 PATH를 한 번의 동의로 설치하고, 계정을 확인한 뒤 끝을 알린다.
/// 준비가 끝났거나 사용자가 [나중에]를 누르면 띄우지 않는다. [나중에]는 앱을 다시 켤 때까지만 유지한다.
export function SetupGuide({ snapshot, onAdd, onReload, request = 0, suspended = false, masked = true }: { snapshot: Snapshot | null; onAdd: (tool: string) => void; onReload: () => Promise<void>; request?: number; suspended?: boolean; masked?: boolean }) {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [open, setOpen] = useState(false);
  const [checkError, setCheckError] = useState<ApiError | null>(null);
  // 공급자 약관과 충돌할 수 있는 선택 기능이라 기본은 꺼짐. 사용자가 체크했을 때만 연결한다(README 고지와 같아야 함).
  const [withOmp, setWithOmp] = useState(false);
  const handledRequest = useRef(0);
  const action = useAction();
  const accounts = snapshot?.accounts.filter(account => ["claude", "codex"].includes(account.tool) && account.enabled && account.canLaunch) ?? [];
  const accountKey = accounts.map(account => account.id).join(",");

  useEffect(() => {
    let skipped = false;
    try { skipped = sessionStorage.getItem(SKIP_KEY) === "1"; } catch { /* 저장소가 막혀도 준비 화면은 보여 준다. */ }
    let active = true;
    if (suspended) return;
    const requested = request > handledRequest.current;
    handledRequest.current = request;
    void setupStatus().then(next => {
      if (!active) return;
      setStatus(next);
      setCheckError(null);
      if ((!next.configured && !skipped) || requested) setOpen(true);
    }).catch(error => {
      if (!active) return;
      setCheckError(toApiError(error));
      if (!skipped || requested) setOpen(true);
    });
    return () => { active = false; };
  }, [accountKey, Boolean(snapshot), request, suspended]);

  if (!open || suspended) return null;
  const skip = () => {
    try { sessionStorage.setItem(SKIP_KEY, "1"); } catch { /* 이번 창에서만 닫는다. */ }
    setOpen(false);
  };
  const check = () => {
    void action.run(async () => {
      const next = await setupCheck();
      setStatus(next);
      setCheckError(null);
      await onReload();
    });
  };
  const run = () => {
    void action.run(async () => {
      const next = await setupInstall(withOmp && Boolean(status?.ompDetected && status.ompSupported));
      setStatus(next);
      setCheckError(null);
      await onReload();
    });
  };
  if (!status) return <Modal title={t("setup.title")} onClose={skip} busy={action.pending}>
    <ActionFeedback error={action.error ?? checkError} message={null} />
    <div className="modal-footer"><button onClick={skip} disabled={action.pending}>{t("setup.later")}</button><button className="primary" onClick={check} disabled={action.pending}>{t("setup.check")}</button></div>
  </Modal>;
  const installed = status.service && status.shell && status.tools.every(tool => !tool.account || tool.connected);
  const hasAccount = status.tools.some(tool => tool.account);
  const notices = status.notices ?? [];
  const forStep = (step: string) => notices.filter(notice => notice.step === step);
  const steps = [
    { id: "service", label: t("setup.stepService"), done: status.service && forStep("service").length === 0 },
    { id: "terminal", label: t("setup.stepTerminal"), done: hasAccount && status.shell && status.tools.every(tool => !tool.account || (tool.connected && tool.verified !== false)) && forStep("terminal").length === 0 },
    { id: "account", label: t("setup.stepAccount"), done: hasAccount && forStep("account").length === 0 },
  ];
  // omp는 선택 기능이다. 연결이 실패해도 Claude·Codex 준비는 완료로 보고, omp에는 경고와 이유를 따로 보여 준다.
  const ompAttempted = status.ompSupported && status.ompDetected && withOmp && Boolean(status.omp);
  const ompFailed = ompAttempted && Boolean(status.omp?.error);
  const ready = status.ready && (!ompAttempted || ompFailed || Boolean(status.omp?.broker && status.omp.bridge && status.omp.observer));

  return <Modal title={ready ? t("setup.doneTitle") : t("setup.title")} subtitle={ready ? undefined : t("setup.subtitle")} onClose={skip} busy={action.pending}>
    <ol className="setup-steps">
      {steps.map(step => <li key={step.id} className={step.done ? "done" : undefined}>
        <div className="setup-step-row">{step.done ? <CheckCircle2 size={18} /> : action.pending ? <Loader2 size={18} className="spinner" /> : <Circle size={18} />}<span>{step.label}</span></div>
        {!step.done && forStep(step.id).map(notice => <p key={`${notice.code}:${notice.tool ?? ""}`} className="field-help warning-text setup-next">{setupNotice(notice)}</p>)}
      </li>)}
    </ol>
    <div className="setup-body">
      {status.tools.map(tool => <p key={tool.tool}>
        <strong>{tool.tool}</strong> · {t(!tool.account ? "setup.stepAccount" : tool.verified === true ? "setup.verified" : tool.verified === false ? "setup.verifyFailed" : tool.connected ? "setup.configured" : "setup.notConnected")}
      </p>)}
      <p className="field-help">{t("setup.verifyHelp")}</p>
      {status.ompDetected && status.ompSupported && <>
        <label><input type="checkbox" checked={withOmp} disabled={action.pending} onChange={event => setWithOmp(event.target.checked)} /> {t("setup.withOmp")}</label>
        <p className="field-help">{t("setup.ompHelp")}</p>
        {status.omp && <p className={status.omp.error ? "warning-text" : undefined}>omp · {t(status.omp.broker && status.omp.bridge && status.omp.observer ? "setup.ompConfigured" : status.omp.error ? "setup.ompFailed" : "setup.notConnected")}{status.omp.error && <> · {status.omp.error}</>}</p>}
      </>}
      {status.ompDetected && !status.ompSupported && <p className="field-help">{t("setup.ompUnsupported")}</p>}
      {ready && <><p>{t("setup.doneBody")}</p><pre className="setup-command">{status.tools.filter(tool => tool.account).map(tool => tool.tool).join("\n")}</pre><p className="field-help">{t("setup.doneHelp")}</p></>}
    </div>
    {!ready && (installed && !hasAccount ? <div className="setup-body">
      <p>{t("setup.noAccount")}</p>
      <div className="setup-actions">
        {(snapshot?.tools ?? []).filter(tool => ["claude", "codex"].includes(tool.id) && tool.installed).map(tool =>
          <button key={tool.id} type="button" className="primary" onClick={() => onAdd(tool.id)}>{t("setup.login", { tool: tool.name })}</button>)}
      </div>
      {!(snapshot?.tools ?? []).some(tool => ["claude", "codex"].includes(tool.id) && tool.installed) && <p className="field-help">{t("setup.noTool")}</p>}
    </div> : hasAccount && <div className="setup-body">
      <p>{t("setup.found", { count: accounts.length })}</p>
      <ul className="setup-accounts">{accounts.map(account => <li key={account.id}>{masked ? t("privacy.accountHidden") : account.email ?? account.label}</li>)}</ul>
    </div>)}
    {!ready && <div className="setup-body"><p className="field-help">{t("setup.details")}</p></div>}
    <ActionFeedback error={action.error ?? checkError} message={null} />
    <div className="modal-footer">
      <button type="button" disabled={action.pending} onClick={skip}>{t(ready ? "setup.doneButton" : "setup.later")}</button>
      <button type="button" disabled={action.pending} onClick={check}>{t("setup.check")}</button>
      {!ready && !(installed && !hasAccount) && <button type="button" className="primary" disabled={action.pending} onClick={run}>{t("setup.start")}</button>}
    </div>
  </Modal>;
}
