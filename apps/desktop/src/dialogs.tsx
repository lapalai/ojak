import { useContext, useEffect, useState } from "react";
import { ArrowUpRight, FolderOpen, LockKeyhole, LogOut, PowerOff, Terminal } from "lucide-react";
import { appInfo, assertOpened, getAutostart, getExpiringNotify, openHomepage, getLanguage, setAutostart, getTrayThreshold, setExpiringNotify, setLanguage, setTrayThreshold, chooseDirectory, contactAuthor, deactivatePlan, installUpdate, launchSession, native, ompBridgeAction, quitApp, restartAfterUpdate, rpc, settingsPreview, updatesStatus } from "./api";
import type { AppInfo, SettingsPreview, UpdateStatus } from "./api";
import ojakIcon from "./assets/ojak-icon.png";
import { ActionFeedback, Badge, Busy, Modal, PrivacyContext, Switch } from "./components";
import { LoginPanel, loginProviderNames } from "./LoginDialog";
import type { LoginRequest } from "./login";
import { privacyText, setCharactersEnabled, toolNames, useAction, useBridge, useCharacters } from "./state";
import type { Account, Decision, LaunchIntent, Snapshot } from "./types";
import { LANGUAGE_KEY, isWindows, locale, t } from "./i18n";
import type { LanguageChoice } from "./i18n";
import { updateControlsVisible, updateInterruptCounts } from "./updates";

export function AddAccountDialog({ snapshot, initialTool, onClose, onReload, onConnected }: {
  snapshot: Snapshot; initialTool?: string; onClose: () => void; onReload: () => Promise<void>; onConnected: (account: Account) => void;
}) {
  const masked = useContext(PrivacyContext);
  const [mode, setMode] = useState<"login" | "profile">("login");
  const [tool, setTool] = useState(initialTool || snapshot.tools.find(item => item.installed && item.isolation !== "observation-only" && item.isolation !== "unsupported")?.id || "claude");
  const [label, setLabel] = useState("");
  const [profilePath, setProfilePath] = useState("");
  const [started, setStarted] = useState<LoginRequest | null>(null);
  const [loginPending, setLoginPending] = useState(false);
  const [preview, setPreview] = useState<SettingsPreview | null>(null);
  const [inheritSettings, setInheritSettings] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const action = useAction();
  const selected = snapshot.tools.find(item => item.id === tool);
  const enrollmentSupported = Boolean(selected && selected.isolation !== "observation-only" && selected.isolation !== "unsupported");
  const loginReady = Boolean(preview && acknowledged && (!inheritSettings || preview.canImport));
  // 로그인을 시작하면 폼 대신 진행 화면을 보인다. 이름과 설정 가져오기는 시작할 때 확정되어 바뀌지 않는다.
  if (started) return <Modal title={t("login.title.new", { provider: loginProviderNames[started.provider] })} subtitle={privacyText(started.label, masked)} onClose={onClose} busy={loginPending}>
    <LoginPanel request={started} autoStart accounts={snapshot.accounts} onReload={onReload} onClose={onClose} onPendingChange={setLoginPending} />
  </Modal>;
  return <Modal title={t("add.title")} subtitle={t("add.subtitle")} onClose={onClose} busy={action.pending}>
    <div className="segmented" aria-label={t("add.modeAria")}>
      <button type="button" aria-pressed={mode === "login"} disabled={action.pending} onClick={() => { setMode("login"); action.clear(); }}>{t("add.mode.login")}</button>
      <button type="button" aria-pressed={mode === "profile"} disabled={action.pending} onClick={() => { setMode("profile"); action.clear(); }}>{t("add.mode.profile")}</button>
    </div>
    <form onSubmit={event => {
      event.preventDefault();
      if (!enrollmentSupported || (mode === "login" && !loginReady)) return;
      void action.run(async () => {
        if (mode === "profile") {
          const account = await rpc<Account>("account.register", { tool, label: label.trim(), profilePath: profilePath.trim() });
          await onReload(); onConnected(account); onClose();
        } else {
          const provider = tool === "claude" ? "anthropic" : tool === "codex" ? "openai-codex" : null;
          if (!provider) throw { code: "ADAPTER_UNVERIFIED", message: t("add.note.unsupported"), retryable: false };
          setStarted({ provider, label: label.trim(), settingsDigest: inheritSettings ? preview?.digest : null });
        }
      });
    }}>
      <fieldset disabled={action.pending} className="form-fields">
        <label>{t("add.tool")}<select value={tool} onChange={event => { setTool(event.target.value); setPreview(null); setAcknowledged(false); setInheritSettings(false); action.clear(); }} autoFocus>{snapshot.tools.map(item => <option key={item.id} value={item.id}>{item.name}{item.isolation === "observation-only" ? t("tool.observationOnly") : item.isolation === "unsupported" ? t("tool.unsupported") : item.installed ? "" : t("tool.installRequired")}</option>)}</select></label>
        {enrollmentSupported && <label>{t("add.label")}<input required maxLength={80} value={label} onChange={event => setLabel(event.target.value)} placeholder={t("add.labelPlaceholder")} autoComplete="off" /></label>}
        {enrollmentSupported && mode === "profile" && <label>{t("add.profilePath")}<div className="input-with-button"><input required value={profilePath} onChange={event => setProfilePath(event.target.value)} placeholder={t("add.profilePlaceholder")} autoComplete="off" spellCheck={false} /><button type="button" onClick={() => { void action.run(async () => { const path = await chooseDirectory(); if (path) setProfilePath(path); }); }}><FolderOpen size={15} />{t("common.choose")}</button></div></label>}
      </fieldset>
      <div className="inline-note"><LockKeyhole size={16} /><p>{!enrollmentSupported ? t("add.note.unsupported") : mode === "login" ? t("add.note.login") : t("add.note.profile")}</p></div>
      {enrollmentSupported && mode === "login" && <section className="settings-preview" aria-label={t("add.previewAria")}>
        <div className="section-heading"><h3>{t("add.previewTitle")}</h3><button type="button" disabled={action.pending || !selected?.installed} onClick={() => {
          void action.run(async () => { const result = await settingsPreview(tool); setPreview(result); setInheritSettings(result.canImport); setAcknowledged(false); });
        }}>{action.pending ? <Busy /> : preview ? t("add.previewAgain") : t("add.preview")}</button></div>
        <p className="field-help">{t("add.previewHelp")}</p>
        {preview && <>
          {preview.source && <p className="field-help path-value">{privacyText(preview.source, masked)}</p>}
          {preview.changes.length > 0 && <div className="settings-diff" aria-label={t("add.diffAria")}>{preview.changes.map(change => <div key={change.key}><code>+ {change.key}</code><pre>{privacyText(JSON.stringify(change.value, null, 2), masked)}</pre></div>)}</div>}
          {preview.warnings.map((warning, index) => <p className="field-help" key={index}>{privacyText(warning, masked)}</p>)}
          {preview.omitted.length > 0 && <ul className="settings-omissions">{preview.omitted.map((item, index) => <li key={index}>{privacyText(item, masked)}</li>)}</ul>}
          <label className="settings-confirm"><input type="checkbox" checked={inheritSettings} disabled={action.pending || !preview.canImport} onChange={event => { setInheritSettings(event.target.checked); setAcknowledged(false); }} /><span>{t("add.inheritLabel")}</span></label>
          <label className="settings-confirm"><input type="checkbox" checked={acknowledged} disabled={action.pending} onChange={event => setAcknowledged(event.target.checked)} /><span>{inheritSettings ? t("add.ackInherit") : t("add.ackFresh")}</span></label>
        </>}
      </section>}
      {selected?.reason && <p className="gate-reason">{privacyText(selected.reason, masked)}</p>}
      {!selected?.installed && <p className="gate-reason">{t("add.notInstalled")}</p>}
      <ActionFeedback error={action.error} message={action.message} />
      <div className="modal-footer"><button type="button" disabled={action.pending} onClick={onClose}>{t("common.close")}</button><button type="submit" className="primary" disabled={!enrollmentSupported || action.pending || !selected?.installed || !label.trim() || (mode === "profile" ? !profilePath.trim() : !loginReady)}>{action.pending ? <Busy /> : mode === "login" ? <><ArrowUpRight size={15} />{t("add.openLogin")}</> : t("add.connectProfile")}</button></div>
    </form>
  </Modal>;
}

export function LaunchDialog({ snapshot, initialAccountId, initialTool, initialModel = "", initialCwd = "", resumeSessionId, masked, onClose, onOpened }: {
  snapshot: Snapshot; initialAccountId?: string; initialTool?: string; initialModel?: string; initialCwd?: string; resumeSessionId?: string; masked: boolean; onClose: () => void; onOpened: () => void;
}) {
  const initialAccount = snapshot.accounts.find(account => account.id === initialAccountId);
  // 관측 전용·미지원 도구는 관리 실행 대상이 아니므로 선택지에서 제외합니다.
  const launchableTools = snapshot.tools.filter(item => item.isolation !== "observation-only" && item.isolation !== "unsupported");
  const [tool, setTool] = useState(initialAccount?.tool || initialTool || launchableTools.find(item => snapshot.accounts.some(account => account.tool === item.id && account.canLaunch && account.enabled))?.id || "claude");
  const [accountId, setAccountId] = useState(initialAccountId || "");
  const [model, setModel] = useState(initialModel === "native-default" ? "" : initialModel);
  const [cwd, setCwd] = useState(initialCwd);
  const [decision, setDecision] = useState<Decision | null>(null);
  const action = useAction();
  const candidates = snapshot.accounts.filter(account => account.tool === tool);
  const requested = candidates.find(account => account.id === accountId);
  const selected = candidates.find(account => account.id === decision?.selectedAccountId);
  const ready = Boolean(tool && cwd.trim() && (accountId ? requested?.canLaunch && requested.enabled : candidates.some(account => account.canLaunch && account.enabled)));
  const canLaunch = ready && selected?.canLaunch && selected.enabled && (!accountId || selected.id === accountId) && decision?.policyRevision === snapshot.policy.revision;
  const models = Array.from(new Set(candidates.flatMap(account => account.buckets.flatMap(bucket => bucket.model ? [bucket.model] : []))));
  const intent: LaunchIntent = { tool, model: model.trim() || "native-default", cwd: cwd.trim(), accountId: accountId || null, parentSessionId: null, resumeSessionId: resumeSessionId || null };
  useEffect(() => { setDecision(null); }, [tool, accountId, model, cwd]);
  return <Modal title={resumeSessionId ? t("sessions.resume") : t("sessions.new")} subtitle={resumeSessionId ? t("launch.resumeSubtitle") : t("launch.subtitle")} onClose={onClose} busy={action.pending} wide>
    <form onSubmit={event => {
      event.preventDefault();
      if (!canLaunch) return;
      void action.run(async () => { assertOpened(await launchSession(intent)); onOpened(); onClose(); });
    }}>
      <fieldset disabled={action.pending || Boolean(resumeSessionId)} className="form-fields">
        <div className="form-columns"><label>{t("common.tool")}<select value={tool} onChange={event => { setTool(event.target.value); setAccountId(""); }} autoFocus>{launchableTools.map(item => <option key={item.id} value={item.id} disabled={!item.installed}>{item.name}{item.installed ? "" : t("tool.notInstalledSuffix")}</option>)}</select></label><label>{t("common.account")}<select value={accountId} onChange={event => setAccountId(event.target.value)}><option value="">{t("launch.autoAccount")}</option>{candidates.map(account => <option key={account.id} value={account.id} disabled={!account.canLaunch || !account.enabled}>{privacyText(account.label, masked)}{account.canLaunch && account.enabled ? "" : ` · ${t("account.launchRestricted")}`}</option>)}</select></label></div>
        <label>{t("launch.model")}<input value={model} onChange={event => setModel(event.target.value)} list="launch-models" placeholder={t("launch.modelPlaceholder")} spellCheck={false} autoComplete="off" /><datalist id="launch-models">{models.map(value => <option key={value} value={value} />)}</datalist><span className="field-help">{t("launch.modelHelp")}</span></label>
        <label>{t("launch.cwd")}<div className="input-with-button"><input required value={cwd} onChange={event => setCwd(event.target.value)} placeholder={t("form.absolutePath")} autoComplete="off" spellCheck={false} /><button type="button" onClick={() => { void action.run(async () => { const path = await chooseDirectory(); if (path) setCwd(path); }); }}><FolderOpen size={15} />{t("common.choose")}</button></div></label>
      </fieldset>
      {requested && !requested.canLaunch && <p className="gate-reason">{privacyText(requested.reason || t("launch.unverifiedReason"), masked)}</p>}
      {requested && !requested.enabled && <p className="gate-reason">{t("launch.excluded")}</p>}
      {!accountId && !snapshot.policy.automatic && <p className="field-help">{t("launch.automaticPaused")}</p>}
      <div className="routing-preview"><div className="section-heading"><h3>{t("launch.explainTitle")}</h3><button type="button" disabled={!ready || action.pending} onClick={() => { void action.run(async () => { const result = await rpc<Decision>("route.explain", { intent }); setDecision(result); }); }}>{action.pending ? <Busy /> : t("launch.explain")}</button></div>
        {decision ? <><p className="decision-summary">{selected ? <><Badge tone="accent">{t("launch.nextSession")}</Badge><strong>{privacyText(selected.label, masked)}</strong><span>{toolNames[selected.tool] || selected.tool}</span></> : t("launch.noEligible")}</p><ul className="candidate-list">{decision.candidates.map(candidate => { const account = snapshot.accounts.find(item => item.id === candidate.accountId); return <li key={candidate.accountId}><span>{privacyText(account?.label || t("account.unregistered"), masked)}</span><Badge tone={candidate.eligible ? "good" : "neutral"}>{candidate.eligible ? t("launch.candidate") : t("launch.excludedBadge")}</Badge><small>{candidate.reasons.length ? privacyText(candidate.reasons.join(" · "), masked) : candidate.eligible ? t("launch.eligibleNow") : t("launch.ineligibleNow")}</small></li>; })}</ul>{decision.policyRevision !== snapshot.policy.revision && <p className="gate-reason">{t("launch.policyChanged")}</p>}</> : <p className="muted">{t("launch.explainHelp")}</p>}
      </div>
      <div className="inline-note"><Terminal size={16} /><p>{resumeSessionId ? t("launch.resumeNote") : t("launch.note")}</p></div>
      <ActionFeedback error={action.error} message={action.message} />
      <div className="modal-footer"><button type="button" disabled={action.pending} onClick={onClose}>{t("common.cancel")}</button><button type="submit" className="primary" disabled={!canLaunch || action.pending}>{action.pending ? <Busy label={t("launch.requesting")} /> : <><Terminal size={15} />{resumeSessionId ? t("launch.resumeButton") : t("launch.start")}</>}</button></div>
    </form>
  </Modal>;
}

export function IntegrationDialog({ action: integration, onClose, onReload }: { action: "install" | "uninstall"; onClose: () => void; onReload: () => Promise<void> }) {
  const action = useAction();
  const [complete, setComplete] = useState(false);
  return <Modal title={integration === "install" ? t("integration.installTitle") : t("integration.uninstallTitle")} subtitle={t("integration.subtitle")} busy={action.pending} onClose={onClose}>
    <div className="approval-summary"><Terminal size={28} /><p>{integration === "install" ? t("integration.installBody") : t("integration.uninstallBody")}</p></div>
    <p className="field-help">{t("integration.helpBefore")}<code>claude</code>·<code>codex</code>{t("integration.helpAfter")}</p>
    <p className="muted">{t("integration.muted")}</p>
    <ActionFeedback error={action.error} message={action.message} />
    <div className="modal-footer"><button disabled={action.pending} onClick={onClose}>{complete ? t("common.close") : t("common.cancel")}</button>{!complete && <button className={integration === "install" ? "primary" : "danger-button"} disabled={action.pending} onClick={() => { void action.run(async () => { const result = await native<string>("integration_action", { action: integration }); await onReload(); setComplete(true); return result; }, result => result); }}>{action.pending ? <Busy /> : integration === "install" ? t("integration.approveInstall") : t("integration.approveUninstall")}</button>}</div>
  </Modal>;
}

export function ServiceDialog({ onClose, onReload }: { onClose: () => void; onReload: () => Promise<void> }) {
  const action = useAction();
  const [complete, setComplete] = useState(false);
  return <Modal title={t("serviceDialog.title")} subtitle={t("serviceDialog.subtitle")} onClose={onClose} busy={action.pending}>
    <p className="dialog-copy">{t("serviceDialog.body")}</p>
    <div className="inline-note"><LockKeyhole size={16} /><p>{t("serviceDialog.note")}</p></div>
    <ActionFeedback error={action.error} message={action.message} />
    <div className="modal-footer"><button disabled={action.pending} onClick={onClose}>{complete ? t("common.close") : t("common.cancel")}</button>{!complete && <button className="primary" disabled={action.pending} onClick={() => { void action.run(async () => { const result = await native<string>("install_service"); await onReload(); setComplete(true); return result; }, result => result); }}>{action.pending ? <Busy label={t("serviceDialog.installing")} /> : t("serviceDialog.approve")}</button>}</div>
  </Modal>;
}

/// 15분 안에 요청이 있었던 omp 브릿지 세션은 사용 중지하면 연결이 끊긴다(bridge.rs ACTIVE_MS와 같은 창).
const ACTIVE_BRIDGE_MS = 15 * 60_000;

export function SettingsDialog({ snapshot, onClose }: { snapshot: Snapshot | null; onClose: () => void }) {
  const masked = useContext(PrivacyContext);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const copy = useAction();
  const [language, setChoice] = useState<LanguageChoice | null>(null);
  const languageAction = useAction();
  const [threshold, setThreshold] = useState<number | null>(null);
  const trayAction = useAction();
  const [autostart, setAutostartState] = useState<boolean | null>(null);
  const autostartAction = useAction();
  const [expiringNotify, setExpiringNotifyState] = useState<boolean | null>(null);
  const notifyAction = useAction();
  useEffect(() => { void getExpiringNotify().then(setExpiringNotifyState).catch(() => setExpiringNotifyState(null)); }, []);
  const changeExpiringNotify = (enabled: boolean) => { void notifyAction.run(async () => setExpiringNotifyState(await setExpiringNotify(enabled))); };
  const characters = useCharacters();
  // 정보의 로고를 7번 누르면 호랑이 쌀가게(이스터에그). 캐릭터 표시를 꺼도 직접 찾아 들어온 이스터에그는 보여 준다.
  const [logoTaps, setLogoTaps] = useState(0);
  // 109KB 그림은 7번째 탭에서만 불러온다. 평소 대시보드 로딩에 포함하지 않는다.
  const [tigerShop, setTigerShop] = useState<string | null>(null);
  const tiger = logoTaps >= 7 && tigerShop !== null;
  useEffect(() => { if (logoTaps === 7 && !tigerShop) void import("./assets/tiger-shop.webp").then(module => setTigerShop(module.default)); }, [logoTaps, tigerShop]);
  const [updates, setUpdates] = useState<UpdateStatus | null>(null);
  const [interrupt, setInterrupt] = useState<{ managed: number; bridge: number } | null>(null);
  const [sessionsUnknown, setSessionsUnknown] = useState(false);
  const updateAction = useAction();
  const restartNeeded = updateAction.error?.code === "SERVICE_RESTART_FAILED" || updateAction.error?.code === "LAUNCHCTL_FAILED" || updateAction.error?.code === "LAUNCHCTL_TIMEOUT";
  useEffect(() => { void getAutostart().then(setAutostartState).catch(() => setAutostartState(null)); }, []);
  const changeAutostart = (enabled: boolean) => { void autostartAction.run(async () => setAutostartState(await setAutostart(enabled))); };
  useEffect(() => { void getTrayThreshold().then(setThreshold).catch(() => setThreshold(30)); }, []);
  // 슬라이더를 움직이는 동안 값이 여러 번 바뀐다. 저장은 설정 파일 한 줄이라 바뀔 때마다 쓴다.
  const saveThreshold = (value: number) => { void setTrayThreshold(value).catch(failure => trayAction.run(() => Promise.reject(failure))); };
  useEffect(() => {
    void appInfo().then(setInfo).catch(() => setInfo(null));
    void getLanguage().then(value => setChoice(value.choice)).catch(() => setChoice("system"));
    void updatesStatus(false).then(setUpdates).catch(() => setUpdates({ enabled: false, available: null, error: null }));
  }, []);
  const changeLanguage = (choice: LanguageChoice) => {
    void languageAction.run(async () => {
      const result = await setLanguage(choice);
      setChoice(choice);
      try {
        if (choice === "system") localStorage.removeItem(LANGUAGE_KEY); else localStorage.setItem(LANGUAGE_KEY, choice);
      } catch { /* 저장소가 막혀도 네이티브 설정은 바뀌었다. */ }
      // 화면 문구는 시작할 때 한 번 정해지므로, 표시 언어가 바뀌면 다시 불러온다.
      if (result.resolved !== locale) window.location.reload();
    });
  };
  const checkUpdates = () => {
    setInterrupt(null);
    setSessionsUnknown(false);
    void updateAction.run(async () => {
      const status = await updatesStatus(true);
      setUpdates(status);
      if (status.error) throw { code: "UPDATE_FAILED", message: status.error, retryable: true };
      return status;
    }, status => status.available ? undefined : t("update.none"));
  };
  const beginInstall = (confirmed: boolean) => {
    void updateAction.run(async () => {
      if (!confirmed) {
        let sessions: { state: string }[] = [];
        let bridgeSessions: { lastUsedAt: number }[] = [];
        let unknown = false;
        try {
          sessions = (await rpc<Snapshot>("status.read")).sessions;
        } catch {
          unknown = true;
        }
        try {
          bridgeSessions = (await ompBridgeAction("status")).bridge?.sessions ?? [];
        } catch {
          unknown = true;
        }
        const counts = updateInterruptCounts(sessions, bridgeSessions, Date.now());
        if (unknown || counts.warn) {
          setInterrupt(counts);
          setSessionsUnknown(unknown);
          return;
        }
      }
      await installUpdate();
    });
  };
  const tools = (snapshot?.tools ?? []).filter(tool => ["claude", "codex", "omp"].includes(tool.id));
  const rows: [string, string][] = info ? [
    [t("about.service"), snapshot ? t("service.running") : t("service.required")],
    ...(snapshot ? [[t("about.serviceStarted"), new Date(snapshot.serviceStartedAt).toLocaleString()] as [string, string]] : []),
    [t("about.protocol"), `v${info.protocolVersion}`],
    [t("about.integration"), `v${info.integrationVersion}`],
    [t("about.platform"), info.platform],
    [t("about.home"), privacyText(info.home, masked)],
    ...tools.map(tool => [tool.name, tool.installed ? tool.version ?? "—" : t("about.toolMissing")] as [string, string]),
  ] : [];
  // 문의용 요약은 가림 설정과 관계없이 경로를 빼고 버전만 담는다.
  const summary = info ? [`Ojak ${info.version} (${info.identifier}, ${info.platform})`, `protocol v${info.protocolVersion} · integration v${info.integrationVersion}`, ...tools.map(tool => `${tool.id} ${tool.installed ? tool.version ?? "?" : "-"}`)].join("\n") : "";
  const warned = Boolean(interrupt) || sessionsUnknown;
  return <Modal title={t("settings.title")} onClose={onClose} busy={updateAction.pending && Boolean(updates?.available)}>
    <h3 className="settings-heading">{t("settings.general")}</h3>
    <div className="settings-row"><div><strong>{t("settings.language")}</strong><p className="field-help">{t("settings.languageHelp")}</p></div>
      <div className="segmented" role="radiogroup" aria-label={t("settings.language")}>{([["system", t("settings.system")], ["ko", "한국어"], ["en", "English"], ["id", "Bahasa Indonesia"]] as [LanguageChoice, string][]).map(([value, label]) => <button key={value} type="button" role="radio" aria-checked={language === value} aria-pressed={language === value} disabled={language === null || languageAction.pending} onClick={() => { if (value !== language) changeLanguage(value); }}>{label}</button>)}</div></div>
    <ActionFeedback error={languageAction.error} message={null} />
    <div className="settings-row"><div><strong>{t("settings.autostart")}</strong><p className="field-help">{t("settings.autostartHelp")}</p></div>
      <Switch checked={autostart ?? false} disabled={autostart === null || autostartAction.pending} label={t("settings.autostart")} onChange={changeAutostart} /></div>
    <ActionFeedback error={autostartAction.error} message={null} />
    {!isWindows && <>
    <div className="settings-row"><div><strong>{t("settings.tray")}</strong><p className="field-help">{t("settings.trayHelp")}</p></div>
      <div className="tray-threshold">
        <input type="range" min={0} max={100} step={5} value={threshold ?? 30} disabled={threshold === null} aria-label={t("settings.tray")}
          onChange={event => { const value = Number(event.target.value); setThreshold(value); saveThreshold(value); }} />
        <output>{threshold === 0 ? t("settings.trayOff") : threshold === 100 ? t("settings.trayAlways") : t("settings.trayBelow", { value: threshold ?? 30 })}</output>
      </div></div>
    <ActionFeedback error={trayAction.error} message={null} />
    </>}
    <div className="settings-row"><div><strong>{t("settings.expiringNotify")}</strong><p className="field-help">{t("settings.expiringNotifyHelp")}</p></div>
      <Switch checked={expiringNotify ?? false} disabled={expiringNotify === null || notifyAction.pending} label={t("settings.expiringNotify")} onChange={changeExpiringNotify} /></div>
    <ActionFeedback error={notifyAction.error} message={null} />
    <div className="settings-row"><div><strong>{t("settings.characters")}</strong><p className="field-help">{t("settings.charactersHelp")}</p></div>
      <Switch checked={characters} label={t("settings.characters")} onChange={setCharactersEnabled} /></div>
    <h3 className="settings-heading">{t("settings.about")}</h3>
    {tiger ? <button type="button" className="tiger-egg" onClick={() => setLogoTaps(0)} aria-label={t("about.tigerClose")}>
      <img src={tigerShop ?? ""} alt={t("about.tigerAlt")} width={160} height={160} />
      <span><strong>{t("about.tigerTitle")}</strong>{t("about.tigerBody")}</span>
    </button> : <div className="about-hero"><img src={ojakIcon} alt="" width={72} height={72} onClick={() => setLogoTaps(count => count + 1)} /><div><strong>Ojak <small>오작</small></strong><span>{info ? t("about.version", { version: info.version }) : "…"}</span><p>{t("about.tagline")}</p><p className="about-origin">{t("about.origin")}</p><p className="about-origin">{t("about.company")} <button type="button" className="link-button" onClick={() => { void copy.run(() => openHomepage()); }}>lio by lapal</button></p></div></div>}
    {updateControlsVisible(updates) && <div className="update-box">
      {updates?.available ? <>
        <p>{t("update.available", { version: updates.available.version })}</p>
        {updates.available.notes ? <><p className="field-label">{t("update.notes")}</p><pre className="update-notes">{updates.available.notes}</pre></> : null}
        {warned && <p className="field-help warning-text">{sessionsUnknown ? t("update.warnUnknown") : t("update.warn", { managed: interrupt?.managed ?? 0, bridge: interrupt?.bridge ?? 0 })}</p>}
      </> : null}
      <ActionFeedback error={updateAction.error} message={updateAction.message} />
      <div className="update-actions">
        <button type="button" disabled={updateAction.pending} onClick={checkUpdates}>{updateAction.pending && !updates?.available ? <Busy label={t("update.checking")} /> : t("update.check")}</button>
        {updates?.available && <button type="button" className="primary" disabled={updateAction.pending} onClick={() => beginInstall(warned)}>{updateAction.pending ? <Busy label={t("update.installing")} /> : warned ? t("update.now") : t("update.install")}</button>}
        {warned && <button type="button" disabled={updateAction.pending} onClick={() => { setInterrupt(null); setSessionsUnknown(false); }}>{t("update.later")}</button>}
        {restartNeeded && <button type="button" disabled={updateAction.pending} onClick={() => { void updateAction.run(() => restartAfterUpdate()); }}>{t("update.restart")}</button>}
      </div>
    </div>}
    <dl className="about-list">{rows.map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value}</dd></div>)}</dl>
    <ActionFeedback error={copy.error} message={copy.message} />
    <div className="modal-footer">{info?.contact && <button type="button" onClick={() => { void contactAuthor(); }}>{t("about.contact")}</button>}<button type="button" className="primary" disabled={!info} onClick={() => { void copy.run(() => navigator.clipboard.writeText(summary), () => t("about.copied")); }}>{t("about.copy")}</button></div>
  </Modal>;
}

export function QuitDialog({ snapshot, onClose }: { snapshot: Snapshot | null; onClose: () => void }) {
  const [step, setStep] = useState<"choose" | "deactivate">("choose");
  const [plan, setPlan] = useState<string | null>(null);
  const action = useAction();
  const { status } = useBridge(null);
  const now = Date.now();
  const ompSessions = status?.bridge?.sessions.filter(session => now - session.lastUsedAt < ACTIVE_BRIDGE_MS).length ?? 0;
  const managed = snapshot ? updateInterruptCounts(snapshot.sessions, [], now).managed : null;
  useEffect(() => {
    if (step !== "deactivate") return;
    void action.run(deactivatePlan, text => { setPlan(text); });
  }, [step]);
  return <Modal title={t("quit.title")} subtitle={step === "choose" ? t("quit.subtitle") : undefined} onClose={onClose} busy={action.pending}>
    {step === "choose" ? <div className="quit-options">
      <button type="button" className="quit-option" onClick={() => { void action.run(() => quitApp("keep")); }} disabled={action.pending}><LogOut size={18} /><span><strong>{t("quit.keep")}</strong><small>{t("quit.keepBody")}</small></span></button>
      <button type="button" className="quit-option danger" onClick={() => setStep("deactivate")} disabled={action.pending}><PowerOff size={18} /><span><strong>{t("quit.deactivate")}</strong><small>{t("quit.deactivateBody")}</small></span></button>
    </div> : <div className="modal-body">
      <p className="field-label">{t("quit.planTitle")}</p>
      {plan ? <pre className="quit-plan">{plan}</pre> : action.pending ? <Busy label={t("quit.planLoading")} /> : null}
      {ompSessions > 0 && <p className="field-help warning-text">{t("quit.omp", { count: ompSessions })}</p>}
      {(managed === null || managed > 0) && <p className="field-help warning-text" role="status">{managed === null ? t("quit.managedUnknown") : t("quit.managedBusy", { count: managed })}</p>}
    </div>}
    <ActionFeedback error={action.error} message={null} />
    <div className="modal-footer">
      {step === "choose" ? <button type="button" onClick={onClose} disabled={action.pending}>{t("common.cancel")}</button> : <>
        <button type="button" onClick={() => { setStep("choose"); setPlan(null); }} disabled={action.pending}>{t("quit.back")}</button>
        <button type="button" disabled={action.pending} onClick={() => { void action.run(() => quitApp("keep")); }}>{t("quit.keep")}</button>
        <button type="button" className="danger-button" disabled={!plan || action.pending || managed === null || managed > 0} onClick={() => { void action.run(() => quitApp("deactivate")); }}>{action.pending && plan ? <Busy label={t("quit.working")} /> : t("quit.confirmDeactivate")}</button>
      </>}
    </div>
  </Modal>;
}
