import { useEffect, useState } from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import { chooseDirectory, rpc } from "./api";
import { ActionFeedback, Busy, Dot, Switch } from "./components";
import { canonicalProvider, groupAccounts, isCurrentGroup, privacyText, projectName, providerNames, toolNames, useAction, useBridge } from "./state";
import type { AccountGroup } from "./state";
import { PROVIDER_ORDER } from "./types";
import type { Account, Policy, ProjectRoute, Snapshot } from "./types";
import { t } from "./i18n";

function groupTitle(group: AccountGroup, masked: boolean): string {
  const provider = providerNames[canonicalProvider(group.primary.provider)];
  const identity = group.email ? masked ? t("privacy.emailHidden") : group.email : privacyText(group.primary.label, masked);
  return `${provider}, ${identity}`;
}

/// 사용 현황 아래 "배정 설정" 안에 들어가는 공통 설정: 자동 배정 방식, 안전 여유량, 계정 순서·배정 포함, 프로젝트 경로.
/// 공급자별 자동/수동과 수동 계정 선택은 사용 현황의 공급자 카드에서 한다.
export function AllocationSettings({ snapshot, online, masked, onReload }: { snapshot: Snapshot; online: boolean; masked: boolean; onReload: () => Promise<void> }) {
  const { policy } = snapshot;
  const action = useAction();
  const { status } = useBridge(null);
  const [reserve, setReserve] = useState(String(policy.safetyReservePercent));
  useEffect(() => { setReserve(String(policy.safetyReservePercent)); }, [policy.safetyReservePercent]);
  const [windowHours, setWindowHours] = useState(String(policy.expiringWindowHours ?? 48));
  const [minPercent, setMinPercent] = useState(String(policy.expiringMinPercent ?? 30));
  useEffect(() => { setWindowHours(String(policy.expiringWindowHours ?? 48)); }, [policy.expiringWindowHours]);
  useEffect(() => { setMinPercent(String(policy.expiringMinPercent ?? 30)); }, [policy.expiringMinPercent]);
  const gatewayKeys = (status?.bridge?.gateways ?? []).map(gateway => `${canonicalProvider(gateway.provider)}:${gateway.email.toLowerCase()}`);
  // 실제로 배정 대상이 되는 계정만 보여 준다: Claude Code·Codex 실행 계정이거나 omp 브릿지 gateway가 열린 계정.
  const groups = groupAccounts(snapshot.accounts).filter(isCurrentGroup).filter(group => group.members.some(member => member.canLaunch) || group.keys.some(key => gatewayKeys.includes(`${canonicalProvider(group.primary.provider)}:${key}`)));
  const priority = policy.accountPriority ?? [];
  const rank = (group: AccountGroup) => Math.min(...group.members.map(member => { const index = priority.indexOf(member.id); return index === -1 ? Infinity : index; }));
  const ordered = [...groups].sort((a, b) => rank(a) - rank(b) || PROVIDER_ORDER.indexOf(canonicalProvider(a.primary.provider)) - PROVIDER_ORDER.indexOf(canonicalProvider(b.primary.provider)) || (a.email ?? "").localeCompare(b.email ?? ""));
  const mode = policy.allocationMode ?? "smart";
  const reserveValid = reserve.trim() !== "" && Number.isFinite(Number(reserve)) && Number(reserve) >= 0 && Number(reserve) <= 100;

  async function update(patch: Record<string, unknown>) {
    try { return await rpc<Policy>("policy.update", { expectedRevision: policy.revision, ...patch }); } finally { await onReload(); }
  }
  function move(index: number, direction: -1 | 1) {
    const target = index + direction;
    if (target < 0 || target >= ordered.length) return;
    const next = [...ordered];
    [next[index], next[target]] = [next[target], next[index]];
    void action.run(() => update({ accountPriority: next.flatMap(group => group.members.map(member => member.id)) }), () => t("policy.orderSaved") + (mode === "priority" ? t("policy.orderSaved.priority") : t("policy.orderSaved.smart")));
  }
  function toggle(group: AccountGroup, enabled: boolean) {
    void action.run(async () => {
      for (const member of group.members) await rpc<Account>("account.update", { id: member.id, enabled });
      await onReload();
    }, () => enabled ? t("policy.included") : t("policy.excluded"));
  }
  function saveReserve() {
    if (!reserveValid || Number(reserve) === policy.safetyReservePercent) return;
    void action.run(() => update({ safetyReservePercent: Number(reserve) }), saved => t("policy.reserveSaved", { percent: saved.safetyReservePercent }));
  }
  function saveExpiring() {
    const hours = Number(windowHours);
    const percent = Number(minPercent);
    const patch: Record<string, number> = {};
    if (Number.isInteger(hours) && hours >= 1 && hours <= 168 && hours !== (policy.expiringWindowHours ?? 48)) patch.expiringWindowHours = hours;
    if (Number.isFinite(percent) && percent >= 1 && percent <= 100 && percent !== (policy.expiringMinPercent ?? 30)) patch.expiringMinPercent = percent;
    if (!Object.keys(patch).length) return;
    void action.run(() => update(patch), saved => t("policy.expiringSaved", { hours: saved.expiringWindowHours ?? 48, percent: saved.expiringMinPercent ?? 30 }));
  }
  return <div className="allocation-settings">
    <p className="sub">{t("policy.subtitle")}</p>
    <div className="panel">
      <header><h2>{t("policy.criteria")}</h2></header>
      <div className="rules">
        <div className="rule"><div className="t">{t("policy.mode")}</div><div className="d">{mode === "smart" ? t("policy.mode.smartDesc") : t("policy.mode.priorityDesc")}</div><div className="ctl"><select aria-label={t("policy.mode")} value={mode} disabled={!online || action.pending} onChange={event => { const allocationMode = event.target.value === "priority" ? "priority" : "smart"; void action.run(() => update({ allocationMode }), () => allocationMode === "priority" ? t("policy.mode.setPriority") : t("policy.mode.setSmart")); }}><option value="smart">{t("policy.mode.smart")}</option><option value="priority">{t("policy.mode.priority")}</option></select></div></div>
        <div className="rule"><div className="t">{t("policy.reserve")}</div><div className="d">{t("policy.reserveDesc")}</div><form className="ctl" onSubmit={event => { event.preventDefault(); saveReserve(); }}><input type="number" min={0} max={100} step={0.5} value={reserve} aria-label={t("policy.reserveAria")} disabled={!online || action.pending} onChange={event => setReserve(event.target.value)} onBlur={saveReserve} />%</form></div>
        <div className="rule"><div className="t">{t("policy.expiring")}</div><div className="d">{t("policy.expiringDesc")}</div><form className="ctl expiring-ctl" onSubmit={event => { event.preventDefault(); saveExpiring(); }}>
          <label>{t("policy.expiringWindow")} <input type="number" min={1} max={168} step={1} value={windowHours} aria-label={t("policy.expiringWindowAria")} disabled={!online || action.pending} onChange={event => setWindowHours(event.target.value)} onBlur={saveExpiring} />{t("policy.hoursUnit")}</label>
          <label>{t("policy.expiringMin")} <input type="number" min={1} max={100} step={5} value={minPercent} aria-label={t("policy.expiringMinAria")} disabled={!online || action.pending} onChange={event => setMinPercent(event.target.value)} onBlur={saveExpiring} />%</label>
        </form></div>
        <div className="rule"><div className="t">{t("policy.expiringBoost")}</div><div className="d">{mode === "smart" ? t("policy.expiringBoostDesc") : t("policy.expiringBoostPriority")}</div><div className="ctl"><Switch checked={policy.expiringBoost ?? false} disabled={!online || action.pending} label={t("policy.expiringBoost")} onChange={expiringBoost => { void action.run(() => update({ expiringBoost }), () => expiringBoost ? t("policy.expiringBoostOn") : t("policy.expiringBoostOff")); }} /></div></div>
        <div className="rule"><div className="t">{t("policy.credits")}</div><div className="d">{t("policy.creditsDesc")} <strong>{t("policy.creditsWarn")}</strong></div><div className="ctl"><Switch checked={policy.useCreditsAfterLimit ?? false} disabled={!online || action.pending} label={t("policy.credits")} onChange={useCreditsAfterLimit => { void action.run(() => update({ useCreditsAfterLimit }), () => useCreditsAfterLimit ? t("policy.creditsOn") : t("policy.creditsOff")); }} /></div></div>
        <div className="rule"><div className="t">{t("policy.extraUsage")}</div><div className="d">{t("policy.extraUsageDesc")} <strong>{t("policy.extraUsageWarn")}</strong></div><div className="ctl"><Switch checked={policy.useExtraUsageAfterLimit ?? false} disabled={!online || action.pending} label={t("policy.extraUsage")} onChange={useExtraUsageAfterLimit => { void action.run(() => update({ useExtraUsageAfterLimit }), () => useExtraUsageAfterLimit ? t("policy.extraUsageOn") : t("policy.extraUsageOff")); }} /></div></div>
        <div className="rule"><div className="t">{t("policy.pinning")}</div><div className="d">{t("policy.pinningDesc")}</div><div className="ctl"><span className="tag">{t("policy.fixed")}</span></div></div>
        <div className="rule"><div className="t">{t("policy.exhausted")}</div><div className="d">{t("policy.exhaustedDesc")}</div><div className="ctl"><span className="tag">{t("policy.fixed")}</span></div></div>
        <div className="rule"><div className="t">{t("policy.stale")}</div><div className="d">{t("policy.staleDesc", { minutes: Math.round(policy.staleAfterSeconds / 60) })}</div><div className="ctl"><span className="tag">{t("policy.fixed")}</span></div></div>
      </div>
    </div>
    <div className="panel">
      <header><h2>{t("policy.orderTitle")}</h2><span className="aside">{t("policy.orderAside")}</span></header>
      {ordered.length ? <ul className="order">{ordered.map((group, index) => {
        const enabled = group.members.every(member => member.enabled);
        return <li key={group.key}>
          <span className="grip"><button type="button" className="icon-button" aria-label={t("policy.moveUp", { name: groupTitle(group, masked) })} disabled={!online || action.pending || index === 0} onClick={() => move(index, -1)}><ChevronUp size={13} /></button><button type="button" className="icon-button" aria-label={t("policy.moveDown", { name: groupTitle(group, masked) })} disabled={!online || action.pending || index === ordered.length - 1} onClick={() => move(index, 1)}><ChevronDown size={13} /></button></span>
          <span><Dot provider={canonicalProvider(group.primary.provider)} className="lead" />{groupTitle(group, masked)}<small className="muted"> · {[...new Set(group.members.map(member => toolNames[member.tool] || member.tool))].join(", ")}{group.members.some(member => !member.canLaunch && member.tool !== "omp") ? ` · ${t("account.launchRestricted")}` : ""}</small></span>
          <span className="muted">{enabled ? t("allocation.included") : t("allocation.excluded")}</span>
          <Switch checked={enabled} disabled={!online || action.pending} label={t("policy.includeLabel", { name: groupTitle(group, masked) })} onChange={next => toggle(group, next)} />
        </li>;
      })}</ul> : <div className="empty">{t("policy.noAccounts")}</div>}
      <div className="row-actions"><span className="hint">{mode === "priority" ? t("policy.orderHint.priority") : t("policy.orderHint.smart")}</span></div>
    </div>
    <ActionFeedback error={action.error} message={action.message} />
    <ProjectRoutesEditor policy={policy} accounts={snapshot.accounts} online={online} masked={masked} onReload={onReload} />
  </div>;
}

function routeAccountName(account: Account | undefined, masked: boolean): string {
  if (!account) return t("account.disconnected");
  return account.email ? masked ? t("privacy.emailHidden") : account.email : privacyText(account.label, masked);
}

function ProjectRoutesEditor({ policy, accounts, online, masked, onReload }: { policy: Policy; accounts: Account[]; online: boolean; masked: boolean; onReload: () => Promise<void> }) {
  const action = useAction();
  const [base, setBase] = useState(policy);
  const [draft, setDraft] = useState<ProjectRoute | null>(null);
  const [editing, setEditing] = useState<number | null>(null);
  useEffect(() => { if (!draft) setBase(policy); }, [policy, draft]);
  const routes = base.projectRoutes ?? [];
  const stale = base.revision !== policy.revision;
  const toolAccounts = accounts.filter(account => account.tool === draft?.tool);
  const selectedAccount = toolAccounts.find(account => account.id === draft?.accountId);
  const duplicate = draft !== null && routes.some((route, index) => index !== editing && route.tool === draft.tool && route.scope === draft.scope && route.path.replace(/\/+$/, "") === draft.path.trim().replace(/\/+$/, ""));
  const valid = draft !== null && draft.path.trim().startsWith("/") && !duplicate && (draft.mode !== "pinned" || Boolean(selectedAccount?.canLaunch && selectedAccount.enabled));
  async function save(projectRoutes: ProjectRoute[]) {
    try {
      const saved = await rpc<Policy>("policy.update", { expectedRevision: base.revision, projectRoutes });
      setBase(saved);
      setDraft(null);
      setEditing(null);
    } finally { await onReload(); }
  }
  const ruleText = (route: ProjectRoute) => route.mode === "pinned" ? t("routes.rule.pinned", { tool: toolNames[route.tool] || route.tool, account: routeAccountName(accounts.find(account => account.id === route.accountId), masked) }) : route.mode === "automatic" ? t("routes.rule.automatic") : t("routes.rule.unmanaged");
  return <div className="panel">
    <header><h2>{t("routes.title")}</h2><span className="aside">{t("routes.aside")}</span></header>
    <table>
      <thead><tr><th>{t("sessions.col.project")}</th><th>{t("routes.col.tool")}</th><th>{t("routes.col.rule")}</th><th></th></tr></thead>
      <tbody>{routes.length ? routes.map((route, index) => <tr key={`${route.scope}:${route.tool}:${route.path}`}>
        <td title={masked ? undefined : route.path}>{projectName(route.path, masked)}<small className="muted"> · {route.scope === "directory" ? t("routes.scope.directory") : t("routes.scope.repository")}</small></td>
        <td><div className="tools"><span className="tag">{toolNames[route.tool] || route.tool}</span></div></td>
        <td>{ruleText(route)}{route.model && ` · ${route.model}`}</td>
        <td className="num"><div className="tools end"><button type="button" className="button" disabled={!online || action.pending || draft !== null} onClick={() => { action.clear(); setEditing(index); setDraft({ ...route }); }}>{t("common.edit")}</button><button type="button" className="button" disabled={!online || action.pending || draft !== null || stale} aria-label={t("routes.removeAria", { project: projectName(route.path, masked), tool: toolNames[route.tool] || route.tool })} onClick={() => { void action.run(() => save(routes.filter((_, current) => current !== index)), () => t("routes.removed")); }}>{t("common.remove")}</button></div></td>
      </tr>) : <tr><td colSpan={4} className="empty">{t("routes.empty")}</td></tr>}</tbody>
    </table>
    {draft && <form className="rule-form" onSubmit={event => {
      event.preventDefault();
      if (!valid || stale) return;
      const route: ProjectRoute = { ...draft, path: draft.path.trim(), accountId: draft.mode === "pinned" ? draft.accountId : null, model: draft.mode === "unmanaged" ? null : draft.model?.trim() || null };
      const next = editing === null ? [...routes, route] : routes.map((value, index) => index === editing ? route : value);
      void action.run(() => save(next), () => t("routes.saved"));
    }}>
      <fieldset className="form-fields" disabled={!online || action.pending}>
        <legend>{editing === null ? t("routes.form.new") : t("routes.form.edit")}</legend>
        <label>{t("routes.form.folder")}<div className="input-with-button"><input required value={draft.path} onChange={event => setDraft({ ...draft, path: event.target.value })} placeholder={t("form.absolutePath")} spellCheck={false} /><button type="button" onClick={() => { void action.run(async () => { const path = await chooseDirectory(); if (path) setDraft({ ...draft, path }); }); }}>{t("common.chooseFolder")}</button></div></label>
        <div className="form-columns"><label>{t("routes.form.scope")}<select value={draft.scope} onChange={event => setDraft({ ...draft, scope: event.target.value === "repository" ? "repository" : "directory" })}><option value="directory">{t("routes.form.scope.directory")}</option><option value="repository">{t("routes.form.scope.repository")}</option></select></label><label>{t("common.tool")}<select value={draft.tool} onChange={event => setDraft({ ...draft, tool: event.target.value, accountId: null, model: null })}><option value="claude">Claude Code</option><option value="codex">Codex</option>{!["claude", "codex"].includes(draft.tool) && <option value={draft.tool}>{toolNames[draft.tool] || draft.tool}</option>}</select></label></div>
        <p className="field-help">{draft.scope === "repository" ? t("routes.form.scopeHelp.repository") : t("routes.form.scopeHelp.directory")}</p>
        <label>{t("routes.form.mode")}<select value={draft.mode} onChange={event => { const mode = event.target.value === "unmanaged" ? "unmanaged" : event.target.value === "automatic" ? "automatic" : "pinned"; setDraft({ ...draft, mode, accountId: mode === "pinned" ? draft.accountId : null, model: mode === "unmanaged" ? null : draft.model }); }}><option value="pinned">{t("routes.form.mode.pinned")}</option><option value="automatic">{t("routes.form.mode.automatic")}</option><option value="unmanaged">{t("routes.rule.unmanaged")}</option></select></label>
        {draft.mode === "pinned" && <label>{t("routes.form.account")}<select required value={draft.accountId || ""} onChange={event => setDraft({ ...draft, accountId: event.target.value || null })}><option value="">{t("routes.form.chooseAccount")}</option>{draft.accountId && !selectedAccount && <option value={draft.accountId} disabled>{t("routes.form.disconnectedAccount")}</option>}{toolAccounts.map(account => <option key={account.id} value={account.id} disabled={!account.canLaunch || !account.enabled}>{routeAccountName(account, masked)} · {privacyText(account.label, masked)}{account.canLaunch && account.enabled ? "" : ` · ${t("account.launchRestricted")}`}</option>)}</select><span className="field-help">{t("routes.form.accountHelp")}</span></label>}
        {draft.mode !== "unmanaged" && <label>{t("routes.form.model")}<input value={draft.model || ""} onChange={event => setDraft({ ...draft, model: event.target.value || null })} placeholder={t("routes.form.modelPlaceholder")} spellCheck={false} /></label>}
      </fieldset>
      {draft.mode === "unmanaged" && <p className="gate-reason">{t("routes.form.unmanagedWarning")}</p>}
      {duplicate && <p className="gate-reason" role="alert">{t("routes.form.duplicate")}</p>}
      {stale && <p className="gate-reason" role="status">{t("routes.form.stale")}</p>}
      <div className="row-actions"><span className="hint" /><button type="button" className="button" disabled={action.pending} onClick={() => { setDraft(null); setEditing(null); setBase(policy); action.clear(); }}>{t("common.cancel")}</button><button className="button primary" type="submit" disabled={!online || action.pending || !valid || stale}>{action.pending ? <Busy /> : t("routes.form.save")}</button></div>
    </form>}
    {!draft && <div className="row-actions"><span className="hint">{t("routes.hint")}</span><button type="button" className="button" disabled={!online || action.pending} onClick={() => { action.clear(); setBase(policy); setEditing(null); setDraft({ path: "", scope: "directory", tool: "claude", mode: "pinned", accountId: null, model: null }); }}>{t("routes.add")}</button></div>}
    <ActionFeedback error={action.error} message={action.message} />
  </div>;
}
