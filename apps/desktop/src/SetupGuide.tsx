import { useEffect, useRef, useState } from "react";
import { CheckCircle2, Circle, Loader2 } from "lucide-react";
import { serviceBusyPreview, serviceRestart, serviceVersionStatus, setupCheck, setupInstall, setupStatus, toApiError } from "./api";
import type { BusyReport, ServiceVersionReport, SetupStatus } from "./api";
import { ActionFeedback, Modal } from "./components";
import { allocatableTools } from "./allocation";
import { allocationLine, codeText, OpenTerminal } from "./OnboardingCard";
import { preferredTool } from "./onboarding";
import { useAction } from "./state";
import type { ApiError, Snapshot } from "./types";
import { setupNotice, t } from "./i18n";
import { busyItems, needsConsent, restartGuidance, toolName } from "./serviceRestart";
import type { BusyItem } from "./serviceRestart";

/// 서비스가 쓰는 중인 세션 때문에 거절하면 화면 언어로 바꿔 보여 준다. 그 밖의 오류는 서비스가 보낸 문장 그대로다.
export function restartFailure(error: ApiError): ApiError {
  return error.code === "SESSION_BUSY" ? { ...error, message: t("serviceVersion.busy"), retryable: true } : error;
}

function itemText(item: BusyItem): string {
  switch (item.kind) {
    case "ompStreams": return t("serviceVersion.item.ompStreams", { count: item.count });
    case "ompQueued": return t("serviceVersion.item.ompQueued", { count: item.count });
    case "ompUnknown": return t("serviceVersion.item.ompUnknown");
    case "terminal": return t("serviceVersion.item.terminal", { tool: toolName(item.tool), count: item.count });
  }
}

/// 앱만 새로 덮어써서 예전 서비스가 남았을 때의 안내. 버튼은 하나이고, 누를 때만 다시 시작한다(조용히 재시작하지 않는다).
/// 누르면 먼저 끊길 작업(omp 응답, Claude·Codex 터미널 대화)을 읽는다. 없으면 바로 다시 시작하고, 있으면 목록을 보여 주는 확인 창을 띄운다.
/// 취소하면 아무것도 바꾸지 않는다. 확인해야만 `force`로 다시 시작한다. 읽지 못한 omp 상태는 '없음'이 아니라 '알 수 없음'으로 보여 준다.
export function ServiceVersionNotice({ snapshot, onReload }: { snapshot: Snapshot | null; onReload: () => Promise<void> }) {
  const [report, setReport] = useState<ServiceVersionReport | null>(null);
  const [confirm, setConfirm] = useState<BusyReport | null>(null);
  const [done, setDone] = useState<{ version: string; busy: BusyReport } | null>(null);
  const action = useAction();
  const startedAt = snapshot?.serviceStartedAt ?? 0;
  const reported = snapshot?.serviceVersion ?? "";

  useEffect(() => {
    if (!snapshot) { setReport(null); return; }
    let active = true;
    void serviceVersionStatus().then(next => { if (active) setReport(next); }).catch(() => { if (active) setReport(null); });
    return () => { active = false; };
  }, [Boolean(snapshot), startedAt, reported]);
  useEffect(() => {
    if (!done || needsConsent(done.busy)) return;
    const timer = window.setTimeout(() => setDone(null), 8000);
    return () => window.clearTimeout(timer);
  }, [done]);

  /// `force`는 확인 창에서 사용자가 누른 경우에만 참이다. 서비스는 끊길 작업이 있으면 `force` 없이는 아무것도 바꾸지 않는다.
  const restart = (force: boolean) => {
    void action.run(async () => {
      if (!force) {
        let preview: BusyReport | null = null;
        try { preview = await serviceBusyPreview(); } catch { preview = null; }
        // 읽지 못해도 서비스가 같은 확인을 한다. 여기서는 읽은 것만 보여 주고, 못 읽었으면 서비스의 거절에 맡긴다.
        if (preview && needsConsent(preview)) { setConfirm(preview); return undefined; }
      }
      setConfirm(null);
      let result;
      try {
        result = await serviceRestart(force);
      } catch (failure) {
        throw restartFailure(toApiError(failure));
      }
      // 동의 없이 호출했는데 그 사이 끊길 작업이 생겼거나 미리보기를 읽지 못했다. 서비스는 아무것도 바꾸지 않았다.
      if (result.status === "busy") { setConfirm(result.busy); return undefined; }
      // 다시 시작했다고 끝이 아니다. 새 서비스가 앱과 같은 버전인지 읽어서 확인한다.
      const next = await serviceVersionStatus();
      setReport(next);
      if (next.mismatch) throw { code: "SERVICE_VERSION_STALE", message: t("serviceVersion.stale"), retryable: true } satisfies ApiError;
      setDone({ version: next.serviceVersion ?? next.appVersion, busy: result.busy });
      await onReload();
      return undefined;
    });
  };

  if (done) {
    const guidance = restartGuidance(done.busy);
    return <div className="update-box service-version-notice" role="status">
      <p><CheckCircle2 size={16} /> {t("serviceVersion.done", { version: done.version })}</p>
      {(guidance.omp || guidance.terminals) && <>
        <p>{t("serviceVersion.after.title")}</p>
        <ul>
          {guidance.omp && <li>{t("serviceVersion.after.omp")}</li>}
          {guidance.omp && <li>{t("serviceVersion.after.ompSent")}</li>}
          {guidance.terminals && <li>{t("serviceVersion.after.terminal")}</li>}
        </ul>
        <div className="update-actions"><button type="button" onClick={() => setDone(null)}>{t("common.close")}</button></div>
      </>}
    </div>;
  }
  if (!report?.mismatch) return null;
  const guidance = confirm ? restartGuidance(confirm) : null;
  return <div className="update-box service-version-notice" role="status">
    <p><strong>{t("serviceVersion.title")}</strong></p>
    <p>{t(report.serviceVersion ? "serviceVersion.body" : "serviceVersion.bodyUnknown", { service: report.serviceVersion ?? "", app: report.appVersion })}</p>
    <ActionFeedback error={action.error} message={null} />
    <div className="update-actions">
      <button type="button" className="primary" disabled={action.pending} onClick={() => restart(false)}>{action.pending ? <Loader2 size={14} className="spinner" /> : null}{t("serviceVersion.restart")}</button>
    </div>
    {confirm && guidance && <Modal title={t("serviceVersion.confirmTitle")} subtitle={t("serviceVersion.confirmBody")} onClose={() => setConfirm(null)} busy={action.pending}>
      <ul className="modal-body">{busyItems(confirm).map(item => <li key={item.kind === "terminal" ? `terminal:${item.tool}` : item.kind}>{itemText(item)}</li>)}</ul>
      {guidance.omp && <p className="field-help warning-text">{t("serviceVersion.confirmOmp")}</p>}
      {guidance.terminals && <p className="field-help">{t("serviceVersion.confirmTerminal")}</p>}
      <ActionFeedback error={action.error} message={null} />
      <div className="modal-footer">
        <button type="button" disabled={action.pending} onClick={() => setConfirm(null)}>{t("common.cancel")}</button>
        <button type="button" className="danger-button" disabled={action.pending} onClick={() => restart(true)}>{action.pending ? <Loader2 size={14} className="spinner" /> : null}{t("serviceVersion.confirmRestart")}</button>
      </div>
    </Modal>}
  </div>;
}

const SKIP_KEY = "ojak.setupSkipped";

/// 첫 실행 준비 화면. 서비스·명령 연결·터미널 PATH를 한 번의 동의로 설치하고, 계정을 확인한 뒤 끝을 알린다.
/// 준비가 끝났거나 사용자가 [나중에]를 누르면 띄우지 않는다. [나중에]는 앱을 다시 켤 때까지만 유지한다.
export function SetupGuide({ snapshot, onAdd, onReload, onChanged, request = 0, suspended = false, masked = true }: { snapshot: Snapshot | null; onAdd: (tool: string) => void; onReload: () => Promise<void>; onChanged?: () => void; request?: number; suspended?: boolean; masked?: boolean }) {
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
  }, [accountKey, Boolean(snapshot), Boolean(snapshot?.refreshing), snapshot?.lastRefreshAt ?? 0, request, suspended]);

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
      onChanged?.();
    });
  };
  const run = () => {
    void action.run(async () => {
      const next = await setupInstall(withOmp && Boolean(status?.ompDetected && status.ompSupported));
      setStatus(next);
      setCheckError(null);
      await onReload();
      onChanged?.();
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
    { id: "service", label: t("setup.stepService"), why: t("setup.whyService"), done: status.service && forStep("service").length === 0 },
    { id: "terminal", label: t("setup.stepTerminal"), why: t("setup.whyTerminal"), done: hasAccount && status.shell && status.tools.every(tool => !tool.account || (tool.connected && tool.verified !== false)) && forStep("terminal").length === 0 },
    { id: "account", label: t("setup.stepAccount"), why: t("setup.whyAccount"), done: hasAccount && forStep("account").length === 0 },
  ];
  // omp는 선택 기능이다. 연결이 실패해도 Claude·Codex 준비는 완료로 보고, omp에는 경고와 이유를 따로 보여 준다.
  const ompAttempted = status.ompSupported && status.ompDetected && withOmp && Boolean(status.omp);
  const ompFailed = ompAttempted && Boolean(status.omp?.error);
  const ready = status.ready && (!ompAttempted || ompFailed || Boolean(status.omp?.broker && status.omp.bridge && status.omp.observer));

  // 터미널은 서비스가 지금 계정을 받을 수 있다고 판단한 도구로만 연다(`canLaunch`는 배정 가능을 뜻하지 않는다).
  const startTool = preferredTool(allocatableTools(status.tools).map(tool => ({ tool, enabled: true, canLaunch: true })));
  // 명령 연결 확인(ready)과 별개로, 서비스가 지금 새 세션을 받을 수 있다고 판단했을 때만 '준비됐어요'라고 말한다.
  const allocatable = status.allocatable;
  return <Modal title={ready ? t(allocatable ? "setup.doneTitle" : "setup.connectedTitle") : t("setup.title")} onClose={skip} busy={action.pending}>
    {!ready && <p className="setup-value">{codeText(t("setup.value"))}</p>}
    <ol className="setup-steps">
      {steps.map(step => <li key={step.id} className={step.done ? "done" : undefined}>
        <div className="setup-step-row">{step.done ? <CheckCircle2 size={18} /> : action.pending ? <Loader2 size={18} className="spinner" /> : <Circle size={18} />}<span>{step.label}</span></div>
        <p className="field-help setup-why">{codeText(step.why)}</p>
        {!step.done && forStep(step.id).map(notice => <p key={`${notice.code}:${notice.tool ?? ""}`} className="field-help warning-text setup-next">{setupNotice(notice)}</p>)}
      </li>)}
    </ol>
    {forStep("service-version").map(notice => <p key={notice.code} className="field-help warning-text setup-next">{setupNotice(notice)}</p>)}
    <div className="setup-body">
      {status.tools.map(tool => <p key={tool.tool}>
        <strong>{tool.tool}</strong> · {t(!tool.account ? "setup.stepAccount" : tool.verified === true ? "setup.verified" : tool.verified === false ? "setup.verifyFailed" : tool.connected ? "setup.configured" : "setup.notConnected")}
        {(tool.account || tool.allocation.state !== "none") && <span className={`field-help${tool.allocation.state === "allocatable" ? "" : " warning-text"}`} data-allocation={tool.allocation.state}> · {allocationLine(tool.allocation)}</span>}
      </p>)}
      <p className="field-help">{t("setup.verifyHelp")}</p>
      {status.ompDetected && status.ompSupported && <>
        <label><input type="checkbox" checked={withOmp} disabled={action.pending} onChange={event => setWithOmp(event.target.checked)} /> {t("setup.withOmp")}</label>
        <p className="field-help">{t("setup.ompHelp")}</p>
        {status.omp && <p className={status.omp.error ? "warning-text" : undefined}>omp · {t(status.omp.broker && status.omp.bridge && status.omp.observer ? "setup.ompConfigured" : status.omp.error ? "setup.ompFailed" : "setup.notConnected")}{status.omp.error && <> · {status.omp.error}</>}</p>}
      </>}
      {status.ompDetected && !status.ompSupported && <p className="field-help">{t("setup.ompUnsupported")}</p>}
      {ready && <>{allocatable && startTool && <OpenTerminal tool={startTool} />}<p className="field-help">{t(allocatable ? "setup.doneHelp" : "setup.connectedHelp")}</p></>}
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
