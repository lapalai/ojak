import { useEffect, useState } from "react";
import { ArrowUpRight, Cable, Download, RefreshCw, Server } from "lucide-react";
import { assertOpened, exportDiagnostics, hostConnections, loginAccount, ompBridgeAction, ompBrokerAction, stopService, toApiError } from "./api";
import type { OmpBridgeStatus, OmpBrokerStatus } from "./api";
import { ActionFeedback, Badge, Busy, Dot, ErrorMessage, Modal } from "./components";
import { absoluteTime, bucketState, canonicalProvider, nextReset, occupiedStates, privacyText, providerNames, relativeTime, toolNames, useAction } from "./state";
import type { Account, AccountQuotaSummary, ApiError, HostConnections, Snapshot } from "./types";
import { hostNote, t } from "./i18n";

const bridgeProviderOrder = ["anthropic", "openai-codex", "google-antigravity", "xai-oauth", "zai"];
/// `crates/service/src/bridge.rs` ALIASES와 같은 표. 하나를 바꾸면 같이 바꾼다.
const ojakLoginId: Record<string, string> = {
  anthropic: "ojak-claude",
  "openai-codex": "ojak-codex",
  "google-antigravity": "ojak-antigravity",
  "xai-oauth": "ojak-grok",
  zai: "ojak-zai",
};
function providerStatus(upstream: string, providers: string[], gateways: { provider: string }[]): string {
  const name = providerNames[canonicalProvider(upstream)];
  if (providers.includes(ojakLoginId[upstream])) return t("omp.provider.loggedIn", { name });
  const hasAccount = providers.includes(upstream) || gateways.some(gateway => gateway.provider === upstream);
  return hasAccount ? t("omp.provider.syncing", { name }) : t("omp.provider.noAccount", { name, upstream });
}


function OmpPanel({ masked }: { masked: boolean }) {
  const [broker, setBroker] = useState<OmpBrokerStatus | null>(null);
  const [bridge, setBridge] = useState<OmpBridgeStatus | null>(null);
  const [statusError, setStatusError] = useState<ApiError | null>(null);
  const action = useAction();
  const refresh = async () => {
    const [nextBroker, nextBridge] = await Promise.all([ompBrokerAction("status"), ompBridgeAction("status")]);
    setBroker(nextBroker); setBridge(nextBridge); setStatusError(null);
  };
  useEffect(() => {
    let active = true;
    void refresh().catch(error => { if (active) setStatusError(toApiError(error)); });
    return () => { active = false; };
  }, []);
  const storeReady = Boolean(broker?.connected && (broker.accountCount ?? 0) > 0);
  const loginReady = Boolean(bridge?.connected && bridge.extensionInstalled);
  const gateways = bridge?.bridge?.gateways ?? [];
  const running = gateways.filter(gateway => gateway.running).length;
  const ojakLogins = (broker?.providers ?? []).filter(provider => provider.startsWith("ojak-")).length;
  const connected = storeReady && loginReady && running > 0;
  const partial = Boolean(broker?.connected || bridge?.connected);
  const counts = bridgeProviderOrder.map(provider => [providerNames[canonicalProvider(provider)], gateways.filter(gateway => gateway.provider === provider).length] as const).filter(([, count]) => count > 0);
  const connect = () => {
    void action.run(async () => {
      if (!storeReady) await ompBrokerAction("connect");
      await ompBridgeAction("connect");
      await refresh();
    }, () => t("omp.connected"));
  };
  const resync = () => {
    void action.run(async () => {
      if (!storeReady) await ompBrokerAction("connect");
      await ompBridgeAction("connect");
      await refresh();
    }, () => t("omp.resynced"));
  };
  const disconnect = () => {
    void action.run(async () => { await ompBridgeAction("disconnect"); await refresh(); }, () => t("omp.disconnected"));
  };
  return <div className="panel">
    <header><Dot color="var(--accent)" /><h2>omp</h2><Badge tone={connected ? "good" : partial ? "warning" : "neutral"}>{connected ? t("status.connected") : partial ? t("status.partial") : broker === null && !statusError ? t("common.checking") : t("status.disconnected")}</Badge></header>
    <div className="steps">
      <div className="step"><div className="t"><Badge tone={storeReady ? "good" : "neutral"}>{storeReady ? t("step.done") : t("step.required")}</Badge>{t("omp.store")}</div><p>{storeReady ? <>{t("omp.store.readyBefore", { count: broker?.accountCount ?? 0 })}<code>/login</code>{t("omp.store.readyAfter")}</> : broker?.connected ? t("omp.store.noAccounts") : t("omp.store.notConnected")}</p></div>
      <div className="step"><div className="t"><Badge tone={loginReady ? "good" : "neutral"}>{loginReady ? t("step.done") : t("step.required")}</Badge>{t("omp.login")}</div><p>{loginReady ? <>{t("omp.login.readyBefore")}<code>/model</code>{t("omp.login.readyAfter")}{ojakLogins > 0 ? t("omp.login.count", { count: ojakLogins }) : t("omp.login.none")}</> : t("omp.login.notReady")}</p>{loginReady && broker && <ul className="field-help">{bridgeProviderOrder.map(upstream => <li key={upstream}>{providerStatus(upstream, broker.providers ?? [], gateways)}</li>)}</ul>}</div>
      <div className="step"><div className="t"><Badge tone={running > 0 ? "good" : "neutral"}>{running > 0 ? t("step.done") : t("step.waiting")}</Badge>{t("omp.gateways", { count: gateways.length })}</div><p>{gateways.length ? <>{counts.map(([name, count]) => `${name} ${count}`).join(", ")}. {running === gateways.length ? t("omp.gateways.allRunning") : t("omp.gateways.stopped", { count: gateways.length - running })}</> : t("omp.gateways.pending")}</p></div>
    </div>
    {bridge?.bridge?.error && <p className="gate-reason">{privacyText(bridge.bridge.error, masked)}</p>}
    {bridge?.bridge?.blocks.length ? <p className="gate-reason">{t("omp.blocked", { list: bridge.bridge.blocks.map(block => t("omp.blockEntry", { who: masked ? t("privacy.accountHidden") : block.email, scope: block.scope ? ` ${block.scope}` : "", reason: block.reason, until: absoluteTime(block.until) })).join(", ") })}</p> : null}
    <div className="row-actions"><span className="hint">{t("omp.hint")}</span><button type="button" className="button" disabled={action.pending} onClick={() => { void action.run(refresh, () => t("omp.refreshed")); }}><RefreshCw size={13} />{t("omp.recheck")}</button>{connected || (storeReady && loginReady) ? <><button type="button" className="button" disabled={action.pending} onClick={resync}>{t("omp.resync")}</button><button type="button" className="button" disabled={action.pending} onClick={disconnect}>{t("common.disconnect")}</button></> : <button type="button" className="button primary" disabled={action.pending || broker === null} onClick={connect}>{action.pending ? <Busy label={t("common.connecting")} /> : t("common.connect")}</button>}</div>
    <ActionFeedback error={action.error || statusError} message={action.message} />
  </div>;
}

function accountState(account: Account, staleAfter: number, summary?: AccountQuotaSummary): { label: string; tone: string } {
  if (!account.canLaunch) return account.authStatus === "auth-required" ? { label: t("account.loginRequired"), tone: "warning" } : account.authStatus === "error" ? { label: t("account.authError"), tone: "warning" } : { label: t("account.launchRestricted"), tone: "neutral" };
  if (!account.enabled) return { label: t("allocation.excluded"), tone: "neutral" };
  // 서비스가 매 스냅샷마다 다시 계산한 값이라 한도가 리셋되면 이 배지는 바로 사라진다.
  if (summary?.kind === "credits" || summary?.kind === "extra") return { label: t(summary.kind === "credits" ? "usage.verdict.credits" : "usage.verdict.extra"), tone: "warning" };
  const exhausted = account.buckets.filter(bucket => bucketState(bucket, staleAfter) === "exhausted");
  if (exhausted.length) {
    const reset = nextReset(exhausted, staleAfter);
    const hint = summary?.extraUsage ? (summary.extraUsage.limitReached ? "" : ` · ${t("usage.extra.hint")}`) : summary?.credits ? ` · ${t("usage.credits.hint")}` : "";
    return { label: `${t("account.exhausted")}${reset ? ` · ${relativeTime(reset, true)}` : ""}${hint}`, tone: "bad" };
  }
  return { label: t("account.ready"), tone: "good" };
}

function ToolPanel({ tool, snapshot, masked, online, shimInstalled, onAdd }: { tool: "claude" | "codex"; snapshot: Snapshot; masked: boolean; online: boolean; shimInstalled: boolean | null; onAdd: (tool: string) => void }) {
  const action = useAction();
  const status = snapshot.tools.find(item => item.id === tool);
  const accounts = snapshot.accounts.filter(account => account.tool === tool);
  const launchable = accounts.filter(account => account.canLaunch && account.enabled).length;
  const relogin = (account: Account) => {
    void action.run(async () => {
      assertOpened(await loginAccount(tool, account.label, null, account.id));
    }, () => t("account.reloginOpened"));
  };
  return <div className="panel">
    <header><Dot provider={tool === "claude" ? "anthropic" : "openai"} /><h2>{toolNames[tool]}</h2><Badge tone={launchable ? "good" : status?.installed ? "warning" : "neutral"}>{!status?.installed ? t("tool.notInstalled") : launchable ? t("status.connected") : accounts.length ? t("tool.noLaunchable") : t("tool.noAccounts")}</Badge></header>
    {accounts.length ? <table><tbody>{accounts.map(account => {
      const state = accountState(account, snapshot.policy.staleAfterSeconds, snapshot.quotaSummaries?.find(summary => summary.accountIds.includes(account.id)));
      const needsLogin = !account.canLaunch && account.authStatus === "auth-required";
      const name = privacyText(account.label, masked);
      return <tr key={account.id}><td>{account.email ? masked ? t("privacy.emailHidden") : account.email : t("account.noEmail")}</td><td className="muted" title={privacyText(account.reason, masked)}>{name}</td><td className="num"><Badge tone={state.tone}>{state.label}</Badge>{needsLogin && <button type="button" className="button" disabled={!online || action.pending || !status?.installed} aria-label={t("account.reloginAria", { name })} onClick={() => relogin(account)}>{t("account.relogin")}</button>}</td></tr>;
    })}</tbody></table> : <div className="empty">{t("connect.noAccounts")}</div>}
    {status?.reason && <p className="gate-reason">{privacyText(status.reason, masked)}</p>}
    <div className="row-actions"><span className="hint">{shimInstalled ? <>{t("tool.shimInstalledBefore")}<code>{tool}</code>{t("tool.shimInstalledAfter")}</> : shimInstalled === false ? <>{t("tool.shimMissingBefore")}<code>{tool}</code>{t("tool.shimMissingAfter")}</> : status?.installed ? t("tool.version", { version: status.version || t("common.unknown") }) : t("tool.installFirst")}</span><button type="button" className="button" disabled={!online || !status?.installed} onClick={() => onAdd(tool)}>{t("tool.addAccount")}</button></div>
    <ActionFeedback error={action.error} message={action.message} />
  </div>;
}

function hostStatusLabel(host: HostConnections["hosts"][number]): string {
  return host.status === "configured" ? t("hosts.status.configured") : host.status === "setup-required" ? t("hosts.status.setupRequired") : host.status === "unsupported" ? t("hosts.status.unsupported") : host.installed ? t("hosts.status.unavailable") : t("tool.notInstalled");
}

function HostsPanel({ connections, loadError, masked, online, onIntegration, onRefresh }: { connections: HostConnections | null; loadError: ApiError | null; masked: boolean; online: boolean; onIntegration: (action: "install" | "uninstall") => void; onRefresh: () => void }) {
  const copy = useAction();
  const installed = connections?.shims.filter(shim => shim.installed).length ?? 0;
  return <div className="panel">
    <header><h2>{t("hosts.title")}</h2><Badge tone={installed ? "good" : "neutral"}>{connections ? installed ? t("hosts.shimCount", { count: installed }) : t("hosts.noShims") : loadError ? t("hosts.checkFailed") : t("common.checking")}</Badge></header>
    {connections && <table><tbody>{connections.hosts.map(host => {
      const [summary, ...more] = host.notes.map(hostNote).filter(Boolean).map(text => privacyText(text, masked));
      return <tr key={host.id} className="host-row"><td>{host.name}</td><td>
        <div className="host-summary">{summary}</div>
        {more.length > 0 && <details className="host-more"><summary>{t("hosts.more")}</summary><ul>{more.map((text, index) => <li key={index}>{text}</li>)}</ul></details>}
      </td><td className="num"><Badge tone={host.status === "configured" ? "good" : host.status === "setup-required" ? "warning" : "neutral"}>{hostStatusLabel(host)}</Badge></td></tr>;
    })}</tbody></table>}
    {connections?.hosts.flatMap(host => host.commands.map((command, index) => <div className="command-reference" key={`${host.id}:${index}`}><span className="muted">{host.name} · {toolNames[command.tool] || command.tool}</span><code>{masked ? t("privacy.pathHidden") : command.command}</code><button type="button" className="button" disabled={copy.pending} onClick={() => { void copy.run(async () => {
      try { await navigator.clipboard.writeText(command.command); } catch { throw { code: "CLIPBOARD_FAILED", message: t("hosts.clipboardFailed"), retryable: true } satisfies ApiError; }
    }, () => t("hosts.copied", { host: host.name, tool: toolNames[command.tool] || command.tool })); }}>{t("common.copy")}</button></div>))}
    <div className="row-actions"><span className="hint">{connections ? <>{t("hosts.hintBefore")}<code>{masked ? t("privacy.pathHidden") : connections.shimDirectory}</code>{t("hosts.hintAfter")}</> : t("hosts.hintLoading")}</span><button type="button" className="button" onClick={onRefresh}><RefreshCw size={13} />{t("common.recheck")}</button><button type="button" className="button" disabled={!online} onClick={() => onIntegration("install")}>{t("hosts.install")}</button><button type="button" className="button" disabled={!online || !installed} onClick={() => onIntegration("uninstall")}>{t("hosts.uninstall")}</button></div>
    <ActionFeedback error={copy.error || loadError} message={copy.message} />
  </div>;
}

export function ConnectionsView({ snapshot, masked, online, refreshing, onRefresh, onReload, onAdd, onService, onIntegration }: {
  snapshot: Snapshot; masked: boolean; online: boolean; refreshing: boolean; onRefresh: () => void; onReload: () => Promise<void>; onAdd: (tool?: string) => void; onService: () => void; onIntegration: (action: "install" | "uninstall") => void;
}) {
  const [confirmStop, setConfirmStop] = useState(false);
  const [exportCancelled, setExportCancelled] = useState(false);
  const [connections, setConnections] = useState<HostConnections | null>(null);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const stop = useAction();
  const diagnostics = useAction();
  const occupied = snapshot.sessions.filter(session => occupiedStates[session.state]).length;
  const loadHosts = () => { void hostConnections().then(result => { setConnections(result); setLoadError(null); }).catch(error => setLoadError(toApiError(error))); };
  useEffect(loadHosts, []);
  const shim = (tool: string) => connections ? connections.shims.some(item => item.tool === tool && item.installed) : null;
  const stopConfirmed = async () => {
    if (await stop.run(stopService, result => result)) {
      setConfirmStop(false);
      await onReload();
    }
  };
  const saveDiagnostics = () => {
    setExportCancelled(false);
    void diagnostics.run(async () => {
      const result = await exportDiagnostics();
      if (result.cancelled && !result.saved) { setExportCancelled(true); return result; }
      if (!result.saved || result.cancelled) throw { code: "EXPORT_NOT_SAVED", message: t("diagnostics.notSaved"), retryable: true } satisfies ApiError;
      return result;
    }, result => result.saved ? t("diagnostics.saved") : undefined);
  };
  return <section className="view" aria-labelledby="connect-title">
    <div className="top"><div><h1 id="connect-title">{t("nav.connect")}</h1><div className="sub">{t("connect.subtitle")}</div></div></div>
    <OmpPanel masked={masked} />
    <ToolPanel tool="claude" snapshot={snapshot} masked={masked} online={online} shimInstalled={shim("claude")} onAdd={onAdd} />
    <ToolPanel tool="codex" snapshot={snapshot} masked={masked} online={online} shimInstalled={shim("codex")} onAdd={onAdd} />
    <HostsPanel connections={connections} loadError={loadError} masked={masked} online={online} onIntegration={onIntegration} onRefresh={loadHosts} />
    <div className="panel">
      <header><Server size={15} className="muted" /><h2>{t("service.title")}</h2><Badge tone={online ? "good" : "warning"}>{online ? t("service.runningShort") : t("service.disconnected")}</Badge></header>
      <table><tbody>
        <tr><td>{t("service.startedAt")}</td><td className="muted">{absoluteTime(snapshot.serviceStartedAt)}</td></tr>
        <tr><td>{t("service.usageRefresh")}</td><td className="muted">{snapshot.lastRefreshAt ? `${relativeTime(snapshot.lastRefreshAt)} · ${absoluteTime(snapshot.lastRefreshAt)}` : t("service.notYet")}</td></tr>
        <tr><td>{t("service.protocol")}</td><td className="muted">{t("service.protocolValue", { version: snapshot.version })}</td></tr>
        <tr><td>{t("service.version")}</td><td className="muted">{snapshot.serviceVersion ?? t("service.versionUnknown")}</td></tr>
      </tbody></table>
      <div className="row-actions"><span className="hint">{t("service.hint")}</span><button type="button" className="button" onClick={onService} disabled={stop.pending}><Download size={13} />{t("service.installStart")}</button><button type="button" className="button" onClick={onRefresh} disabled={refreshing || stop.pending}>{refreshing ? <Busy /> : <><RefreshCw size={13} />{t("service.refreshUsage")}</>}</button><button type="button" className="button" onClick={() => { stop.clear(); setConfirmStop(true); }} disabled={!online || stop.pending}>{t("service.stop")}</button></div>
      <div className="row-actions"><span className="hint">{t("diagnostics.hint")}</span><button type="button" className="button" onClick={saveDiagnostics} disabled={!online || diagnostics.pending || stop.pending}>{diagnostics.pending ? <Busy label={t("diagnostics.preparing")} /> : t("diagnostics.export")}</button></div>
      {exportCancelled && <p className="field-help" role="status">{t("diagnostics.cancelled")}</p>}
      {snapshot.notices.length > 0 && <ul className="diagnostic-notices">{snapshot.notices.map(notice => <li key={notice.id}><Badge tone={notice.level === "error" ? "bad" : notice.level === "warning" ? "warning" : "neutral"}>{notice.level === "error" ? t("notice.error") : notice.level === "warning" ? t("notice.warning") : t("notice.info")}</Badge><div><strong>{privacyText(notice.title, masked)}</strong><p>{masked && notice.id === "observed-session-discovery" ? t("notice.maskedDiscovery") : privacyText(notice.message, masked)}</p></div></li>)}</ul>}
      {!confirmStop && <ActionFeedback error={stop.error} message={stop.message} />}
      <ActionFeedback error={diagnostics.error} message={diagnostics.message} />
    </div>
    {confirmStop && <Modal title={t("service.stopTitle")} subtitle={t("service.stopSubtitle")} onClose={() => setConfirmStop(false)} busy={stop.pending}>
      <div className="modal-body"><p className="body-copy">{t("service.stopBody")}</p><p className="field-help">{t("service.stopOccupied", { count: occupied })}</p><ActionFeedback error={stop.error} message={null} /></div>
      <div className="modal-footer"><button onClick={() => setConfirmStop(false)} disabled={stop.pending}>{t("common.cancel")}</button><button className="danger-button" onClick={() => { void stopConfirmed(); }} disabled={!online || stop.pending || occupied > 0}>{stop.pending ? <Busy label={t("service.stopping")} /> : t("service.stopConfirm")}</button></div>
    </Modal>}
  </section>;
}

export function ServiceUnavailable({ error, connecting, onRetry, onInstall }: { error: ApiError | null; connecting: boolean; onRetry: () => void; onInstall: () => void }) {
  const browserOnly = error?.code === "DESKTOP_REQUIRED";
  return <div className="service-unavailable"><div className="service-illustration"><Server size={42} strokeWidth={1.3} /><span><Cable size={17} /></span></div><h2>{connecting ? t("unavailable.connecting") : browserOnly ? t("unavailable.browser") : t("unavailable.title")}</h2><p>{connecting ? t("unavailable.connectingBody") : t("unavailable.body")}</p>{connecting ? <Busy label={t("unavailable.waiting")} /> : <><ErrorMessage error={error} /><div className="button-row">{!browserOnly && <button className="primary" onClick={onInstall}><Download size={15} />{t("unavailable.install")}</button>}<button onClick={onRetry}><RefreshCw size={15} />{t("offline.retry")}</button></div><details className="connection-help"><summary>{t("unavailable.help")}<ArrowUpRight size={13} /></summary><ul><li>{t("unavailable.help1")}</li><li>{t("unavailable.help2")}</li><li>{t("unavailable.help3")}</li><li>{t("unavailable.help4")}</li></ul></details></>}</div>;
}
