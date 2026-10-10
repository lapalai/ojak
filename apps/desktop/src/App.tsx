import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { isTauri } from "@tauri-apps/api/core";
import { ChevronRight, Eye, EyeOff, RefreshCw, Settings, Terminal } from "lucide-react";
import { ConnectionsView, ServiceUnavailable } from "./ConnectionsView";
import { SessionsView } from "./SessionsView";
import { UsageView } from "./UsageView";
import { SetupGuide, ServiceVersionNotice } from "./SetupGuide";
import { SettingsDialog, AddAccountDialog, IntegrationDialog, LaunchDialog, QuitDialog, ServiceDialog } from "./dialogs";
import { ActionFeedback, ErrorMessage, PrivacyContext, Switch } from "./components";
import { appInfo, getLanguage, refreshIntegrations, rpc, setPrivacy, updatesStatus } from "./api";
import ojakIcon from "./assets/ojak-icon.png";
import { groupAccounts, isCurrentGroup, occupiedStates, relativeTime, useAction, useSnapshot } from "./state";
import type { Policy, Session } from "./types";
import { LANGUAGE_KEY, locale, modKey, t } from "./i18n";
import { UPDATE_CHECK_INTERVAL_MS } from "./updates";

type View = "usage" | "sessions" | "connect";
type Dialog = { kind: "add"; tool?: string } | { kind: "launch"; accountId?: string; tool?: string; model?: string; cwd?: string; resumeSessionId?: string } | { kind: "service" } | { kind: "integration"; action: "install" | "uninstall" } | { kind: "settings" } | { kind: "quit" };
const navigation: { id: View; label: string }[] = [
  { id: "usage", label: t("nav.usage") },
  { id: "sessions", label: t("nav.sessions") },
  { id: "connect", label: t("nav.connect") },
];

export default function App() {
  const { snapshot, error, connecting, refreshing, reload } = useSnapshot();
  const [view, setView] = useState<View>("usage");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [setupRequest, setSetupRequest] = useState(0);
  const [setupRevision, setSetupRevision] = useState(0);
  const [launchMessage, setLaunchMessage] = useState<string | null>(null);
  const [version, setVersion] = useState<string | null>(null);
  const [masked, setMasked] = useState(() => { try { return localStorage.getItem("aam.privacy") !== "visible"; } catch { return true; } });
  const policyAction = useAction();
  const integrationAction = useAction();
  const online = Boolean(snapshot && !error);
  const accountCount = snapshot ? groupAccounts(snapshot.accounts).filter(isCurrentGroup).length : null;
  const sessionCount = snapshot ? snapshot.sessions.filter(session => occupiedStates[session.state]).length + (snapshot.observedSessions?.length ?? 0) : null;
  const onReload = useCallback(() => reload(), [reload]);
  const refresh = useCallback(() => { void reload(true); }, [reload]);
  const onAdd = (tool?: string) => setDialog({ kind: "add", tool });
  const onNewSession = (session?: Session) => setDialog({ kind: "launch", tool: session?.tool, model: session?.model, cwd: session?.cwd });
  const onResume = (session: Session) => setDialog({ kind: "launch", accountId: session.accountId, tool: session.tool, model: session.model, cwd: session.cwd, resumeSessionId: session.id });

  useEffect(() => {
    try { localStorage.setItem("aam.privacy", masked ? "masked" : "visible"); } catch { /* 저장이 제한되어도 현재 창의 마스킹은 유지합니다. */ }
    if (isTauri()) void setPrivacy(masked).catch(() => undefined);
  }, [masked]);
  useEffect(() => {
    if (!isTauri()) return;
    void appInfo().then(info => setVersion(info.version)).catch(() => setVersion(null));
    void integrationAction.run(refreshIntegrations);
    const updateTimer = window.setInterval(() => { void updatesStatus(false).catch(() => undefined); }, UPDATE_CHECK_INTERVAL_MS);
    void updatesStatus(false).catch(() => undefined);
    // 네이티브 언어 설정이 기준이다. 화면이 다른 언어로 시작했으면(저장소 초기화 등) 맞춰 다시 불러온다.
    void getLanguage().then(({ choice, resolved }) => {
      if (resolved === locale) return;
      try { if (choice === "system") localStorage.removeItem(LANGUAGE_KEY); else localStorage.setItem(LANGUAGE_KEY, choice); } catch { return; }
      window.location.reload();
    }).catch(() => undefined);
    // 트레이·⌘Q·Dock 종료 요청은 네이티브가 막고 이 이벤트로 확인 창을 띄운다.
    const unlisten = [listen("ojak://confirm-quit", () => setDialog({ kind: "quit" })), listen("ojak://settings", () => setDialog({ kind: "settings" }))];
    return () => { window.clearInterval(updateTimer); for (const stop of unlisten) void stop.then(off => off()); };
  }, []);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      // macOS는 ⌘, Windows는 Ctrl이 단축키 수정자다.
      const primary = modKey === "Ctrl" ? event.ctrlKey && !event.metaKey : event.metaKey && !event.ctrlKey;
      if (!primary || event.altKey) return;
      const key = event.key.toLowerCase();
      if (!["1", "2", "3", "r"].includes(key)) return;
      event.preventDefault();
      if (document.querySelector("dialog[open]")) return;
      if (key === "r") { refresh(); return; }
      setView(navigation[Number(key) - 1].id);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [refresh]);

  const busy = refreshing || Boolean(snapshot?.refreshing);
  return <PrivacyContext.Provider value={masked}><div className="window">
    <aside className="sidebar" aria-label={t("nav.aria")}>
      <div className="sidebar-titlebar" data-tauri-drag-region />
      <div className="brand" data-tauri-drag-region><img src={ojakIcon} alt="" width={28} height={28} draggable={false} /><span>Ojak<small>오작</small></span></div>
      <nav className="nav">{navigation.map((item, index) => <button type="button" key={item.id} aria-current={view === item.id ? "page" : undefined} onClick={() => setView(item.id)} title={`${item.label} (${modKey === "Ctrl" ? "Ctrl+" : "⌘"}${index + 1})`}>{item.label}{item.id === "usage" && accountCount !== null && <small>{accountCount}</small>}{item.id === "sessions" && sessionCount !== null && <small>{sessionCount}</small>}</button>)}</nav>
      <div className="side-foot">
        <div className="row"><span className={online ? "live" : `dot ${connecting ? "waiting" : "off"}`} />{online ? t("service.running") : connecting ? t("service.connecting") : t("service.required")}</div>
        <div className="row">{t("policy.automatic")} <Switch checked={Boolean(snapshot?.policy.automatic)} disabled={!online || policyAction.pending} label={t("policy.automatic")} onChange={automatic => { if (!snapshot) return; void policyAction.run(async () => { try { return await rpc<Policy>("policy.update", { expectedRevision: snapshot.policy.revision, automatic }); } finally { await onReload(); } }, policy => policy.automatic ? t("policy.automaticOn") : t("policy.automaticOff")); }} /></div>
        <div className="row muted"><span>{busy ? t("usage.refreshing") : snapshot?.lastRefreshAt ? t("usage.refreshedAt", { time: relativeTime(snapshot.lastRefreshAt) }) : t("usage.neverRefreshed")}</span><button type="button" className="icon-button" onClick={refresh} disabled={!online || busy} title={t("refresh.title")} aria-label={t("refresh.aria")}><RefreshCw size={13} className={busy ? "spinner" : undefined} /></button></div>
        <button type="button" className="privacy-toggle" onClick={() => setMasked(!masked)} aria-pressed={masked} title={t("privacy.toggleTitle")}>{masked ? <EyeOff size={14} /> : <Eye size={14} />}<span>{masked ? t("privacy.masked") : t("privacy.visible")}</span></button>
        <button type="button" className="privacy-toggle" onClick={() => setSetupRequest(value => value + 1)}>{t("setup.title")}</button>
        <button type="button" className="privacy-toggle" onClick={() => setDialog({ kind: "settings" })} title={`${t("settings.open")} (${modKey === "Ctrl" ? "Ctrl+" : "⌘"},)`}><Settings size={14} /><span>{t("settings.open")}</span></button>
        <div className="credit"><button type="button" className="version-link" onClick={() => setDialog({ kind: "settings" })} title={t("about.open")}>Ojak {version ?? ""}</button><span>{t("about.madeBy", { name: "lio" })}</span></div>
      </div>
    </aside>
    <main className="main-pane">
      <div className="main-titlebar" data-tauri-drag-region />
      {!snapshot ? <ServiceUnavailable error={error} connecting={connecting} onRetry={() => { void reload(); }} onInstall={() => setDialog({ kind: "service" })} /> : <div className="workspace-scroll" key={view}><div className="main-content">
        {error && <div className="offline-notice"><ErrorMessage error={error} /><p>{t("offline.notice")}</p><button type="button" className="button" onClick={() => { void reload(); }}>{t("offline.retry")}</button></div>}
        <ActionFeedback error={policyAction.error} message={policyAction.message} />
        <ActionFeedback error={integrationAction.error} message={integrationAction.message} />
        {isTauri() && <ServiceVersionNotice snapshot={snapshot} onReload={onReload} />}
        {launchMessage && <div className="launch-notice" role="status"><Terminal size={16} /><span>{launchMessage}</span><button type="button" className="text-button" onClick={() => { setView("sessions"); setLaunchMessage(null); }}>{t("launch.viewSessions")}<ChevronRight size={13} /></button></div>}
        {view === "usage" && <UsageView snapshot={snapshot} masked={masked} online={online} refreshing={busy} onRefresh={refresh} onReload={onReload} onConnect={() => setView("connect")} onSessions={() => setView("sessions")} onAdd={onAdd} onSetup={() => setSetupRequest(value => value + 1)} setupRequest={setupRequest} setupRevision={setupRevision} />}
        {view === "sessions" && <SessionsView snapshot={snapshot} masked={masked} online={online} onNewSession={onNewSession} onResume={onResume} />}
        {view === "connect" && <ConnectionsView snapshot={snapshot} masked={masked} online={online} refreshing={busy} onRefresh={refresh} onReload={onReload} onAdd={onAdd} onService={() => setDialog({ kind: "service" })} onIntegration={action => setDialog({ kind: "integration", action })} />}
      </div></div>}
    </main>
    {dialog?.kind === "service" && <ServiceDialog onClose={() => setDialog(null)} onReload={onReload} />}
    {dialog?.kind === "integration" && <IntegrationDialog action={dialog.action} onClose={() => setDialog(null)} onReload={onReload} />}
    {dialog?.kind === "add" && snapshot && <AddAccountDialog snapshot={snapshot} initialTool={dialog.tool} onClose={() => setDialog(null)} onReload={onReload} onConnected={() => setView("connect")} />}
    {dialog?.kind === "launch" && snapshot && <LaunchDialog snapshot={snapshot} initialAccountId={dialog.accountId} initialTool={dialog.tool} initialModel={dialog.model} initialCwd={dialog.cwd} resumeSessionId={dialog.resumeSessionId} masked={masked} onClose={() => setDialog(null)} onOpened={() => { setLaunchMessage(t("launch.opened")); void onReload(); }} />}
    {isTauri() && <SetupGuide snapshot={snapshot} onAdd={tool => onAdd(tool)} onReload={onReload} onChanged={() => setSetupRevision(value => value + 1)} request={setupRequest} suspended={dialog !== null} masked={masked} />}
    {dialog?.kind === "settings" && <SettingsDialog snapshot={snapshot} onClose={() => setDialog(null)} />}
    {dialog?.kind === "quit" && <QuitDialog snapshot={snapshot} onClose={() => setDialog(null)} />}
  </div></PrivacyContext.Provider>;
}
