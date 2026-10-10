import { useCallback, useContext, useEffect, useRef, useState } from "react";
import { ArrowUpRight, Check, Circle, LoaderCircle } from "lucide-react";
import { ActionFeedback, Busy, ErrorMessage, Modal, PrivacyContext } from "./components";
import { cancelProviderLogin, listProviderLogins, providerLoginStatus, startProviderLogin, toApiError } from "./api";
import { canStartLogin, findLoginJob, isTerminalLogin, loginFailure, loginStep, mergeLoginStatus, reflectedInSnapshot } from "./login";
import type { LoginProvider, LoginRequest, LoginStatus } from "./login";
import { privacyText, useAction } from "./state";
import { t } from "./i18n";
import type { MessageKey } from "./i18n";
import type { Account } from "./types";

/// 공급자 이름은 번역하지 않는 제품명이다.
export const loginProviderNames: Record<LoginProvider, string> = {
  anthropic: "Claude",
  "openai-codex": "Codex",
  "xai-oauth": "xAI (Grok)",
  "google-antigravity": "Google Antigravity",
};

const POLL_MS = 1000;
const STEPS: readonly [MessageKey, number][] = [["login.step.browser", 0], ["login.step.verify", 1], ["login.step.sync", 2]];

/// 이 앱 세션에서 화면에 결과(성공·실패·취소)를 이미 보여 준 작업. 창을 닫았다 다시 열 때 이미 본 결과가 새 로그인을 가리지 않게 한다.
const seenResults = new Set<string>();

interface LoginPanelProps {
  request: LoginRequest;
  /// 바로 로그인을 시작한다(계정 추가 폼에서 사용자가 이미 시작을 눌렀을 때). 아니면 시작 버튼을 보이고, 이미 진행 중인 같은 작업이 있으면 이어 붙인다.
  autoStart?: boolean;
  accounts: readonly Pick<Account, "id" | "tool" | "authStatus" | "canLaunch" | "verification">[];
  online?: boolean;
  onReload: () => Promise<void>;
  onClose: () => void;
  /// 시작·취소 요청이 오가는 동안 true. 부모 모달이 닫기를 막는 데 쓴다.
  onPendingChange?: (pending: boolean) => void;
}

/// 앱이 직접 여는 공식 로그인 한 건의 진행 화면. 브라우저를 열었다는 사실이 아니라 네이티브 작업의 상태(`succeeded`는 계정 확인 뒤에만)로 보여 준다.
/// 닫아도 작업은 계속된다. 다시 열면 같은 공급자·계정의 가장 최근 작업에 이어 붙는다: 진행 중이면 진행 화면, 창이 닫힌 사이 끝났고 아직 보지 못한 결과(성공·실패·취소)는 그 결과 화면이다. 이미 본 결과나 오래된 결과는 새 로그인 화면이다. 취소는 이 화면의 버튼으로만 한다.
export function LoginPanel({ request, autoStart = false, accounts, online = true, onReload, onClose, onPendingChange }: LoginPanelProps) {
  const masked = useContext(PrivacyContext);
  const [job, setJob] = useState<LoginStatus | null>(null);
  const [attaching, setAttaching] = useState(true);
  const [reattached, setReattached] = useState(false);
  const [pollTrouble, setPollTrouble] = useState(false);
  const start = useAction();
  const cancel = useAction();
  const sync = useAction();
  const autoStarted = useRef(false);
  const syncedJob = useRef<string | null>(null);
  const provider = loginProviderNames[request.provider];
  const relogin = Boolean(request.accountId);
  const target = request.accountName ? privacyText(request.accountName, masked) : null;
  const state = job?.state ?? null;
  const active = job !== null && !isTerminalLogin(job.state);
  const jobId = job?.id ?? null;
  const reflected = job?.state === "succeeded" && reflectedInSnapshot(job, accounts);
  const stepAt = job ? loginStep(job.state) : null;

  const begin = () => {
    void start.run(async () => {
      const next = await startProviderLogin(request);
      setJob(next);
      setReattached(false);
      setPollTrouble(false);
    });
  };

  // 처음 열 때: 시작 요청이면 바로 시작하고(항상 새 작업), 아니면 같은 공급자·계정의 최근 작업(진행 중 → 아직 못 본 결과)에 이어 붙인다.
  useEffect(() => {
    if (autoStart) {
      if (!autoStarted.current) { autoStarted.current = true; begin(); }
      setAttaching(false);
      return;
    }
    let alive = true;
    void listProviderLogins()
      .then(list => {
        const match = findLoginJob(list, request, seenResults);
        if (alive && match) { setJob(match); setReattached(true); }
      })
      .catch(() => { /* 목록을 못 읽어도 새로 시작할 수 있다. 같은 작업이 있으면 시작 응답이 그 작업을 돌려준다. */ })
      .finally(() => { if (alive) setAttaching(false); });
    return () => { alive = false; };
  }, []);

  // 진행 중인 작업만 폴링한다. 응답이 겹치지 않게 한 번 끝난 뒤 다음 요청을 예약한다.
  const accept = useCallback((next: LoginStatus) => setJob(prev => mergeLoginStatus(prev, next)), []);
  useEffect(() => {
    if (!jobId || !active) return;
    let alive = true;
    let timer = 0;
    let failures = 0;
    const tick = async () => {
      try {
        const next = await providerLoginStatus(jobId);
        if (!alive) return;
        failures = 0;
        setPollTrouble(false);
        accept(next);
      } catch (failure) {
        if (!alive) return;
        const error = toApiError(failure);
        if (error.code === "LOGIN_JOB_NOT_FOUND") {
          // 결과를 알 수 없다. 성공으로 보이지 않고, 계정 목록을 확인하라고 안내한다.
          setJob(prev => prev && prev.id === jobId && !isTerminalLogin(prev.state) ? { ...prev, state: "failed", accountId: null, error: { ...error, retryable: true } } : prev);
          return;
        }
        failures += 1;
        if (failures >= 3) setPollTrouble(true);
      }
      if (alive) timer = window.setTimeout(tick, POLL_MS);
    };
    timer = window.setTimeout(tick, POLL_MS);
    return () => { alive = false; window.clearTimeout(timer); };
  }, [jobId, active, accept]);

  // 확인이 끝난 뒤에만 스냅샷을 다시 읽는다.
  useEffect(() => {
    if (job?.state !== "succeeded" || syncedJob.current === job.id) return;
    syncedJob.current = job.id;
    void sync.run(onReload);
  }, [job?.state, job?.id]);

  // 결과 화면을 보여 준 작업은 본 것으로 기록한다. 다시 열었을 때 같은 결과를 되풀이하지 않는다.
  useEffect(() => {
    if (job && isTerminalLogin(job.state)) seenResults.add(job.id);
  }, [job?.state, job?.id]);

  const pending = start.pending || cancel.pending;
  useEffect(() => { onPendingChange?.(pending); }, [pending]);
  useEffect(() => () => onPendingChange?.(false), []);

  const stopLogin = () => {
    if (!job) return;
    void cancel.run(async () => { accept(await cancelProviderLogin(job.id)); });
  };
  const errors = [start.error, cancel.error, sync.error].filter((error): error is NonNullable<typeof error> => error !== null);
  const facts = target || (relogin && request.workspace) ? <dl className="login-facts">
    {target && <><dt>{t("login.account")}</dt><dd>{target}</dd></>}
    {relogin && request.workspace && <><dt>{t("login.workspace")}</dt><dd>{privacyText(request.workspace, masked)}</dd></>}
  </dl> : null;

  return <div className="login-panel">
    {attaching && !start.pending ? <p className="field-help"><Busy /></p> : !job ? <>
      <p className="dialog-copy">{t(relogin ? "login.intro.relogin" : "login.intro.new", { provider })}</p>
      {facts}
      <p className="field-help">{relogin ? (request.workspace ? t("login.workspaceHint") : t("login.sameAccountHint")) : t("login.chooseHint")}</p>
      {start.pending && <p className="login-status" role="status"><LoaderCircle size={15} className="spinner" aria-hidden="true" />{t("login.state.starting")}</p>}
    </> : <>
      {facts}
      {reattached && active && <p className="field-help">{t("login.reattached")}</p>}
      {stepAt !== null && <ol className="login-steps" aria-label={t("login.steps.aria")}>{STEPS.map(([key, index]) => {
        const status = stepAt > index ? "done" : stepAt === index ? "active" : "todo";
        return <li key={key} data-state={status} aria-current={status === "active" ? "step" : undefined}>
          {status === "done" ? <Check size={14} aria-hidden="true" /> : status === "active" ? <LoaderCircle size={14} className="spinner" aria-hidden="true" /> : <Circle size={14} aria-hidden="true" />}
          <span>{t(key)}</span>
        </li>;
      })}</ol>}
      <p className="login-status" role="status" data-state={job.state}>
        {active && <LoaderCircle size={15} className="spinner" aria-hidden="true" />}
        {job.state === "succeeded" && <Check size={15} aria-hidden="true" />}
        <span>{t(`login.state.${job.state}` as MessageKey)}</span>
      </p>
      {job.state === "succeeded" && job.identity && <p className="login-identity">{job.identity.workspace
        ? t("login.identityWorkspace", { identity: privacyText(job.identity.label, masked), workspace: privacyText(job.identity.workspace, masked) })
        : t("login.identity", { identity: privacyText(job.identity.label, masked) })}</p>}
      {job.state === "succeeded" && <p className="field-help" role="status">{reflected ? t("login.reflected") : sync.pending ? t("login.reflecting") : t("login.notReflected")}</p>}
      {pollTrouble && active && <p className="field-help warning-text" role="status">{t("login.pollFailed")}</p>}
      {job.state === "failed" && job.error && <ErrorMessage error={loginFailure(job.error)} />}
      {active && <p className="field-help">{t("login.closeHint")}</p>}
    </>}
    {!online && !job && <p className="gate-reason">{t("login.offline")}</p>}
    {errors.map(error => <ActionFeedback key={`${error.code}:${error.message}`} error={loginFailure(error)} message={null} />)}
    <div className="modal-footer">
      <button type="button" onClick={onClose}>{t("common.close")}</button>
      {!job && !attaching && <button type="button" className="primary" disabled={!online || start.pending || !canStartLogin(request)} onClick={begin}>{start.pending ? <Busy label={t("login.starting")} /> : <><ArrowUpRight size={15} />{t("login.start")}</>}</button>}
      {job && (state === "starting" || state === "waiting" || state === "verifying") && <button type="button" disabled={cancel.pending} onClick={stopLogin}>{cancel.pending ? <Busy label={t("login.canceling")} /> : t("login.cancel")}</button>}
      {job?.state === "succeeded" && !reflected && <button type="button" className="primary" disabled={sync.pending} onClick={() => { void sync.run(onReload); }}>{sync.pending ? <Busy /> : t("common.recheck")}</button>}
      {job?.state === "canceled" && <button type="button" className="primary" disabled={!online || start.pending} onClick={begin}>{t("login.startAgain")}</button>}
      {job?.state === "failed" && job.error?.retryable !== false && <button type="button" className="primary" disabled={!online || start.pending} onClick={begin}>{t("login.retry")}</button>}
    </div>
  </div>;
}

/// 계정 한 줄·연결 화면에서 여는 로그인 창(다시 로그인, xAI·Google 로그인). 새 Claude·Codex 계정은 이름·설정 가져오기를 정한 뒤 계정 추가 창이 같은 진행 화면을 쓴다.
export function ProviderLoginDialog({ request, accounts, online, onReload, onClose }: Omit<LoginPanelProps, "autoStart" | "onPendingChange">) {
  const [pending, setPending] = useState(false);
  const provider = loginProviderNames[request.provider];
  return <Modal title={t(request.accountId ? "login.title.relogin" : "login.title.new", { provider })} onClose={onClose} busy={pending}>
    <LoginPanel request={request} accounts={accounts} online={online} onReload={onReload} onClose={onClose} onPendingChange={setPending} />
  </Modal>;
}
