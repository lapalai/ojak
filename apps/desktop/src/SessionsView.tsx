import { useState } from "react";
import { ArrowUpRight, Terminal } from "lucide-react";
import { Badge, Dot } from "./components";
import { absoluteTime, canonicalProvider, occupiedStates, privacyText, projectName, relativeTime, sessionLabel, toolNames, useBridge, verificationLabel } from "./state";
import type { Session, Snapshot } from "./types";
import { t } from "./i18n";

interface Row { id: string; cwd: string | null; tool: string; model: string; provider: string; account: string | null; requests: number | null; lastUsed: number; state: string | null; session: Session | null }

export function SessionsView({ snapshot, masked, online, onNewSession, onResume }: {
  snapshot: Snapshot; masked: boolean; online: boolean; onNewSession: (session?: Session) => void; onResume: (session: Session) => void;
}) {
  const [filter, setFilter] = useState<"active" | "all">("active");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const { status } = useBridge(null);
  const bridgeRows: Row[] = (status?.bridge?.sessions ?? []).map(session => ({
    id: `omp:${session.session}`, cwd: session.cwd, tool: "omp", model: session.model, provider: canonicalProvider(session.provider), account: session.email, requests: session.requests, lastUsed: session.lastUsedAt, state: null, session: null,
  }));
  const managedRows: Row[] = snapshot.sessions.filter(session => filter === "all" || occupiedStates[session.state]).map(session => {
    const account = snapshot.accounts.find(item => item.id === session.accountId);
    return { id: session.id, cwd: session.cwd, tool: session.tool, model: session.model === "native-default" ? t("session.defaultModel") : session.model, provider: account ? canonicalProvider(account.provider) : "other", account: account?.email ?? account?.label ?? null, requests: null, lastUsed: session.updatedAt, state: session.state, session };
  });
  const rows = [...bridgeRows, ...managedRows].sort((a, b) => b.lastUsed - a.lastUsed);
  const selected = snapshot.sessions.find(session => session.id === selectedId);
  const selectedAccount = snapshot.accounts.find(account => account.id === selected?.accountId);
  return <section className="view" aria-labelledby="sessions-title">
    <div className="top">
      <div><h1 id="sessions-title">{t("nav.sessions")}</h1><div className="sub">{t("sessions.subtitle")}{!online && t("sessions.offlineSuffix")}</div></div>
      <div className="row-actions inline"><div className="segmented" role="group" aria-label={t("sessions.filterAria")}><button type="button" aria-pressed={filter === "active"} onClick={() => setFilter("active")}>{t("sessions.filter.active")}</button><button type="button" aria-pressed={filter === "all"} onClick={() => setFilter("all")}>{t("sessions.filter.all")}</button></div><button type="button" className="button" disabled={!online} onClick={() => onNewSession()}><Terminal size={14} />{t("sessions.new")}</button></div>
    </div>
    <div className="panel"><table>
      <thead><tr><th>{t("sessions.col.project")}</th><th>{t("sessions.col.tool")}</th><th>{t("sessions.col.model")}</th><th>{t("sessions.col.account")}</th><th className="num">{t("sessions.col.requests")}</th><th>{t("sessions.col.lastUsed")}</th></tr></thead>
      <tbody>{rows.length ? rows.map(row => <tr key={row.id} aria-selected={row.session ? selectedId === row.id : undefined} className={row.session ? "selectable" : undefined} onClick={row.session ? () => setSelectedId(selectedId === row.id ? null : row.id) : undefined}>
        <td title={row.cwd && !masked ? row.cwd : undefined}>{projectName(row.cwd, masked)}</td>
        <td><div className="tools"><span className="tag">{toolNames[row.tool] || row.tool}</span>{row.state && <Badge tone={row.state === "ACTIVE" ? "good" : ["SUSPECT", "ORPHANED", "FAILED"].includes(row.state) ? "warning" : "neutral"}>{sessionLabel(row.state)}</Badge>}</div></td>
        <td><Dot provider={row.provider} className="lead" />{row.model}</td>
        <td>{row.account ? row.account.includes("@") ? privacyText(row.account, masked) : masked ? t("privacy.accountHidden") : row.account : t("account.unknown")}</td>
        <td className="num">{row.requests === null ? <span className="muted" title={t("sessions.noRequestCount")}>—</span> : row.requests}</td>
        <td className="muted" title={absoluteTime(row.lastUsed)}>{relativeTime(row.lastUsed)}</td>
      </tr>) : <tr><td colSpan={6} className="empty">{filter === "active" ? t("sessions.empty.active") : t("sessions.empty.all")}{status && !status.connected && t("sessions.empty.bridgeSuffix")}</td></tr>}</tbody>
    </table>
    <div className="row-actions"><span className="hint">{t("sessions.hint")}</span></div></div>
    {selected && <div className="panel session-inspector">
      <header><h2>{t("sessions.detailTitle", { tool: toolNames[selected.tool] || selected.tool })}</h2><button type="button" className="text-button" onClick={() => setSelectedId(null)}>{t("common.collapse")}</button></header>
      <dl className="detail-list">
        <dt>{t("sessions.detail.id")}</dt><dd className="path-value">{selected.id}</dd>
        <dt>{t("sessions.detail.account")}</dt><dd>{privacyText(selectedAccount?.label || t("account.unregistered"), masked)}</dd>
        <dt>{t("sessions.detail.verification")}</dt><dd>{verificationLabel(selected.verification)}</dd>
        {selected.parentSessionId && <><dt>{t("sessions.detail.parent")}</dt><dd className="path-value">{selected.parentSessionId}</dd></>}
        <dt>{t("sessions.detail.cwd")}</dt><dd className="path-value">{masked ? t("privacy.cwdHidden") : selected.cwd}</dd>
        <dt>{t("sessions.detail.process")}</dt><dd>{selected.process ? `PID ${selected.process.pid}` : t("sessions.detail.notReported")}</dd>
        <dt>{t("sessions.detail.started")}</dt><dd>{absoluteTime(selected.startedAt)}</dd>
        <dt>{t("sessions.detail.updated")}</dt><dd>{absoluteTime(selected.updatedAt)}</dd>
        {selected.exitCode !== null && <><dt>{t("sessions.detail.exitCode")}</dt><dd>{selected.exitCode}</dd></>}
        {selected.reason && <><dt>{t("sessions.detail.reason")}</dt><dd>{privacyText(selected.reason, masked)}</dd></>}
      </dl>
      <div className="row-actions"><span className="hint">{t("sessions.resumeHint")}</span>
        {selected.tool === "claude" && selected.nativeSessionId && selected.state === "EXITED" && <button type="button" className="button" disabled={!online || !selectedAccount?.canLaunch || !selectedAccount.enabled} title={!selectedAccount?.canLaunch ? privacyText(selectedAccount?.reason || t("sessions.reconnectFirst"), masked) : undefined} onClick={() => onResume(selected)}><Terminal size={14} />{t("sessions.resume")}</button>}
        <button type="button" className="button" disabled={!online} onClick={() => onNewSession(selected)}><ArrowUpRight size={14} />{t("sessions.newWithSameSettings")}</button></div>
    </div>}
  </section>;
}
