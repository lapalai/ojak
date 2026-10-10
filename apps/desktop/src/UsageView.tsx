import { useEffect, useMemo, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import type { CSSProperties } from "react";
import { ActionFeedback, Badge, Dot, ErrorMessage, Sparkline } from "./components";
import { AccountRow, accountText } from "./AccountRow";
import { AllocationSettings } from "./AllocationSettings";
import { FirstSuccessNotice, OnboardingCard } from "./OnboardingCard";
import { getRecoveryWatches, rpc, setRecoveryWatch, toApiError } from "./api";
import { canonicalProvider, fullTime, groupAccounts, isCurrentGroup, projectName, providerNames, relativeTime, timeZoneLabel, useAction, useBridge, useCharacters, useWindowVisible } from "./state";
import type { AccountGroup } from "./state";
import { PROVIDER_ORDER } from "./types";
import type { Account, ApiError, Policy, Snapshot } from "./types";
import { t } from "./i18n";
import { observedRouteModels } from "./usage-routes";
import { allResting } from "./limits";
import { compareRows, groupAlias, isExpanded, quotaRows, toggleExpanded } from "./overview";
import type { UsageRow } from "./overview";
import tigerStrip from "./assets/tiger-strip.webp";
import tigerNap from "./assets/tiger-nap.webp";

type Period = "15" | "60" | "today";
const periods: Period[] = ["15", "60", "today"];

function roleLabel(role: string): string {
  return role === "main" ? t("usage.role.main") : role === "subagent" ? t("usage.role.subagent") : role === "auxiliary" ? t("usage.role.auxiliary") : t("usage.role.unknown");
}

/// 수동 배정 키는 화면의 공급자 묶음 이름(서비스 `pin_provider`와 같은 규칙)이다.
/// 그룹 안에서 이 공급자 키로 고정할 계정. 서비스가 계정의 공급자와 키를 대조하므로 맞는 구성원을 고른다.
function pinMember(group: AccountGroup, provider: string): Account | undefined {
  return group.members.find(member => canonicalProvider(member.provider) === provider) ?? undefined;
}

/// 펼침을 기억하는 ID. 계정 묶음은 구성원 ID, 묶음이 없는 줄은 줄 키다.
const rowIds = (row: UsageRow): string[] => row.group ? row.group.members.map(member => member.id) : [row.key];

export function UsageView({ snapshot, masked, online, refreshing, onReload, onRefresh, onConnect, onSessions, onAdd, onSetup, setupRequest, setupRevision }: { snapshot: Snapshot; masked: boolean; online: boolean; refreshing: boolean; onReload: () => Promise<void>; onRefresh: () => void; onConnect: () => void; onSessions: () => void; onAdd: (tool: string) => void; onSetup: () => void; setupRequest: number; setupRevision: number }) {
  const pinAction = useAction();
  const characters = useCharacters();
  const pins = snapshot.policy.providerPins ?? {};
  /// 수동 배정 계정을 바꾼다. `accountId`가 null이면 그 공급자를 자동 배정으로 돌린다.
  function setPin(provider: string, accountId: string | null) {
    const next: Record<string, string> = {};
    // 서비스는 모든 항목의 계정이 있는지 검사하므로 이미 사라진 계정의 항목은 함께 정리한다.
    for (const [key, id] of Object.entries(pins)) if (key !== provider && snapshot.accounts.some(account => account.id === id)) next[key] = id;
    if (accountId) next[provider] = accountId;
    void pinAction.run(async () => { try { return await rpc<Policy>("policy.update", { expectedRevision: snapshot.policy.revision, providerPins: next }); } finally { await onReload(); } },
      () => accountId ? t("usage.pin.saved", { provider: providerNames[provider] }) : t("usage.pin.auto", { provider: providerNames[provider] }));
  }
  const pinnedGroup = (provider: string, row: UsageRow) => Boolean(row.group && pins[provider] && row.group.members.some(member => member.id === pins[provider]));
  const [open, setOpen] = useState<ReadonlySet<string>>(() => new Set());
  const [watched, setWatched] = useState<string[] | null>(null);
  const [watchError, setWatchError] = useState<ApiError | null>(null);
  const watchAvailable = isTauri();
  const visible = useWindowVisible();
  // 알림 감시는 서비스가 한도 회복을 확인하면 스스로 사라지므로 주기적으로 다시 읽는다. 창이 숨겨지면 멈춘다.
  useEffect(() => {
    if (!watchAvailable || !visible) return;
    let active = true;
    const load = () => { if (document.hidden) return; getRecoveryWatches().then(ids => { if (active) { setWatched(ids); setWatchError(null); } }).catch(failure => { if (active) setWatchError(toApiError(failure)); }); };
    load();
    const interval = window.setInterval(load, 5000);
    return () => { active = false; window.clearInterval(interval); };
  }, [watchAvailable, visible]);
  const watch = useMemo(() => ({
    available: watchAvailable, watched,
    set: async (accountId: string, enabled: boolean) => { const next = await setRecoveryWatch(accountId, enabled); setWatched(next); setWatchError(null); return next; },
  }), [watchAvailable, watched]);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => { if (!visible) return; setNow(Date.now()); const timer = window.setInterval(() => setNow(Date.now()), 30_000); return () => window.clearInterval(timer); }, [visible]);
  const [period, setPeriod] = useState<Period>("60");
  // 오늘은 0시부터 지금까지를 5분 단위로 올림한다. 5분마다 한 구간씩 늘어난다.
  const midnight = new Date(); midnight.setHours(0, 0, 0, 0);
  const windowMinutes = period === "today" ? Math.max(5, Math.ceil((Date.now() - midnight.getTime()) / 300_000) * 5) : Number(period);
  const { status, usage: reportedUsage, error, loaded } = useBridge(windowMinutes);
  const usage = error ? null : reportedUsage;
  const bridge = status?.bridge ?? null;
  const reserve = snapshot.policy.safetyReservePercent;
  const staleAfter = snapshot.policy.staleAfterSeconds;

  const { rows, peak, directCount, unknownCount, routeDetails } = useMemo(() => {
    const sessionCwd: Record<string, string> = {};
    for (const session of bridge?.sessions ?? []) if (session.cwd) sessionCwd[session.session] = session.cwd;
    const rows: UsageRow[] = groupAccounts(snapshot.accounts).filter(isCurrentGroup).map(group => ({
      key: group.key, provider: canonicalProvider(group.primary.provider), account: group.email, group, cells: [], quotas: quotaRows(group.buckets, staleAfter, now), verdict: null, detail: [], notes: [], off: group.members.every(member => !member.enabled), unknown: false,
      alias: groupAlias(group.members), sortKey: "",
    }));
    const find = (provider: string, account: string) => rows.find(row => row.provider === provider && (row.group ? row.group.keys.includes(account.toLowerCase()) : row.account?.toLowerCase() === account.toLowerCase()));
    const synthesize = (provider: string, account: string): UsageRow => {
      const row: UsageRow = { key: `${provider}:${account}`, provider, account, group: null, cells: [], quotas: [], verdict: null, detail: [], notes: [t("usage.note.unregisteredBridgeAccount")], off: false, unknown: false, alias: null, sortKey: "" };
      rows.push(row);
      return row;
    };
    const matchedGateway = new Set<string>();
    for (const gateway of bridge?.gateways ?? []) {
      const provider = canonicalProvider(gateway.provider);
      const row = find(provider, gateway.email) ?? synthesize(provider, gateway.email);
      matchedGateway.add(row.key);
      if (!gateway.running) row.notes.push(t("usage.note.gatewayStopped"));
    }
    for (const bucket of usage?.buckets ?? []) {
      const provider = canonicalProvider(bucket.provider);
      const row = find(provider, bucket.account) ?? synthesize(provider, bucket.account);
      // 보조 요청만 있는 모델은 대화 요청 수·순위에 포함하지 않는다.
      if (bucket.total === 0) continue;
      const projects = [...new Set(bucket.sessions.map(id => sessionCwd[id]).filter((cwd): cwd is string => Boolean(cwd)))];
      row.cells.push({ key: `${bucket.provider}:${bucket.model}`, name: bucket.model, series: bucket.series, total: bucket.total, now: bucket.series[bucket.series.length - 1] ?? 0, projects, route: "bridge", roles: [] });
    }
    // 응답 당시 경로만 확정한다. 로그 누락·폴더 일치·원래 공급자 이름으로 경로를 추정하지 않는다.
    const to = Date.now();
    const observed = observedRouteModels(snapshot.observedSessions ?? [], to - windowMinutes * 60_000, to, canonicalProvider);
    for (const model of observed) {
      const key = `${model.provider}:${model.route}`;
      const row = rows.find(item => item.key === key) ?? (() => {
        const created: UsageRow = { key, provider: model.provider, account: null, group: null, cells: [], quotas: [], verdict: null, detail: [], notes: [t(model.route === "direct" ? "usage.note.directCall" : "usage.note.routeUnknown")], off: false, unknown: true, alias: null, sortKey: "" };
        rows.push(created);
        return created;
      })();
      row.cells.push({ key: model.key, name: model.model, series: [], total: 0, now: 0, projects: model.projects, route: model.route, roles: model.roles.map(roleLabel) });
    }
    for (const row of rows) {
      row.cells.sort((a, b) => b.now - a.now || b.total - a.total);
      if (row.group && !row.quotas.length && !row.unknown) row.notes.push(t("usage.note.noQuota"));
      if (row.off) row.notes.push(t("allocation.excluded"));
      // 공용 한도(모델 없음)를 먼저, 모델 전용 한도를 그 아래에 둔다. 화면은 모델 전용을 들여 써서 하위 한도로 보여 준다.
      row.quotas.sort((a, b) => Number(Boolean(a.bucket.model)) - Number(Boolean(b.bucket.model)));
      const blocks = (bridge?.blocks ?? []).filter(block => canonicalProvider(block.provider) === row.provider && row.account && block.email.toLowerCase() === row.account.toLowerCase());
      // 원인 해석 없는 차단 원문(범위·사유·시각)은 배지 툴팁으로만 보여 준다.
      row.detail = blocks.map(block => `${block.scope ?? "*"} · ${block.reason} · ${fullTime(block.until)}`);
      if (row.group && !row.unknown) {
        row.verdict = snapshot.quotaSummaries?.find(summary => summary.accountIds.includes(row.group!.primary.id))
          ?? { accountIds: row.group.members.map(member => member.id), kind: "unknown", until: null, models: [], label: null, rate: false };
        if (!snapshot.quotaSummaries) row.notes.push(t("api.protocolMismatch"));
      }
    }
    // 순서는 이름으로만 정한다. 사용량으로 정렬하면 값이 바뀔 때마다 줄이 뛴다.
    for (const row of rows) row.sortKey = (row.alias ?? row.account ?? "").toLowerCase();
    rows.sort(compareRows);
    const peak = Math.max(1, ...rows.flatMap(row => row.cells.flatMap(cell => cell.series)));
    const routeDetails = observed.flatMap(model => model.observations.map(observation => ({
      ...observation, key: JSON.stringify([model.key, observation.project]), provider: model.provider, model: model.model, route: model.route,
    }))).sort((a, b) => b.lastRecordedAt - a.lastRecordedAt || a.key.localeCompare(b.key));
    return { rows, peak, directCount: observed.filter(model => model.route === "direct").length, unknownCount: observed.filter(model => model.route === "unknown").length, routeDetails };
  }, [snapshot.accounts, snapshot.quotaSummaries, snapshot.observedSessions, bridge, usage, windowMinutes, reserve, staleAfter, now]);

  const lines = rows.flatMap(row => row.cells.filter(cell => cell.route === "bridge").map(cell => ({ row, cell }))).sort((a, b) => b.cell.now - a.cell.now || b.cell.total - a.cell.total);
  const lead = lines[0];
  const periodLabel = t(`usage.period.${period}`);
  const usageUnavailable = Boolean(error) || !loaded || !usage;
  const bridgeOn = Boolean(status?.connected);

  return <section className="view" aria-labelledby="usage-title">
    <div className="top">
      <div><h1 id="usage-title">{t("nav.usage")}</h1><div className="sub">{bridgeOn ? t("usage.subtitle", { period: periodLabel }) : t("usage.subtitle.accounts")}</div></div>
      {bridgeOn && <div className="segmented" role="group" aria-label={t("usage.periodAria")}>{periods.map(value => <button type="button" key={value} aria-pressed={period === value} onClick={() => setPeriod(value)}>{t(`usage.periodShort.${value}`)}</button>)}</div>}
    </div>
    {online && <FirstSuccessNotice snapshot={snapshot} masked={masked} />}
    {online && isTauri() && <OnboardingCard snapshot={snapshot} request={setupRequest} revision={setupRevision} onAdd={onAdd} onSetup={onSetup} />}
    <div className="notice"><p>{bridgeOn ? t("usage.scope") : t("usage.scope.accounts")} <button type="button" className="text-button" onClick={onSessions}>{t("nav.sessions")}</button> · <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>
    <details className="settings-strip">
      <summary>{t("usage.settings.summary", { mode: (snapshot.policy.allocationMode ?? "smart") === "priority" ? t("policy.mode.priority") : t("policy.mode.smart"), reserve })}</summary>
      <AllocationSettings snapshot={snapshot} masked={masked} online={online} onReload={onReload} />
    </details>
    <ActionFeedback error={pinAction.error} message={pinAction.message} />
    <ErrorMessage error={watchError} />
    {error && bridgeOn && <ErrorMessage error={error} />}
    {bridgeOn && directCount > 0 && <div className="notice" style={{ "--tone": "var(--warning)" } as CSSProperties}><p><b>{t("usage.direct.title", { count: directCount })}</b> {t("usage.direct.bodyBefore")}<code>/model</code>{t("usage.direct.bodyAfter")}</p></div>}
    {bridgeOn && unknownCount > 0 && <div className="notice"><p><b>{t("usage.routeUnknown.title", { count: unknownCount })}</b> {t("usage.routeUnknown.body")} <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>}
    {PROVIDER_ORDER.map(provider => {
      const providerRows = rows.filter(row => row.provider === provider);
      if (providerRows.length === 0) return null;
      const total = providerRows.reduce((sum, row) => sum + row.cells.filter(cell => cell.route === "bridge").reduce((inner, cell) => inner + cell.total, 0), 0);
      const pickable = providerRows.filter(row => row.group && !row.unknown && pinMember(row.group, provider));
      const manual = Boolean(pins[provider]);
      const pinnedRow = providerRows.find(row => pinnedGroup(provider, row));
      // 잔여 한도는 대안이 없으면 쓴다. 여유 있는 다른 계정이 있을 때만 우선순위에서 밀린다.
      const pinFallback = manual && (!pinnedRow || (pinnedRow.verdict && (
        !["available", "partial", "reserve"].includes(pinnedRow.verdict.kind)
        || (pinnedRow.verdict.kind === "reserve" && pickable.some(row => row !== pinnedRow && row.verdict?.kind === "available"))
      )));
      const resting = allResting(providerRows.filter(row => row.verdict && !row.unknown).map(row => ({ kind: row.verdict!.kind, until: row.verdict!.until, off: row.off })));
      return <section className="panel" key={provider} aria-label={providerNames[provider]}>
        <header><Dot provider={provider} /><h2>{providerNames[provider]}</h2>{bridgeOn && <span className="aside">{usageUnavailable ? t(error ? "usage.readFailed" : "common.checking") : total ? t("usage.periodTotal", { period: periodLabel, count: t("usage.requestCount", { count: total }) }) : t("usage.noRecentCalls")}</span>}
          {pickable.length > 0 && <div className="segmented small" role="radiogroup" aria-label={t("usage.pin.modeAria", { provider: providerNames[provider] })}>
            <button type="button" role="radio" aria-checked={!manual} aria-pressed={!manual} disabled={!online || pinAction.pending} onClick={() => { if (manual) setPin(provider, null); }}>{t("usage.pin.modeAuto")}</button>
            <button type="button" role="radio" aria-checked={manual} aria-pressed={manual} disabled={!online || pinAction.pending} onClick={() => { if (!manual) { const first = pickable.find(row => row.verdict && ["available", "partial", "reserve"].includes(row.verdict.kind)) ?? pickable[0]; const member = first.group && pinMember(first.group, provider); if (member) setPin(provider, member.id); } }}>{t("usage.pin.modeManual")}</button>
          </div>}
        </header>
        {pinFallback && <div className="notice pin-fallback" style={{ "--tone": "var(--warning)" } as CSSProperties}><p>{t("usage.pin.fallback")} <button type="button" className="text-button" disabled={!online || pinAction.pending} onClick={() => setPin(provider, null)}>{t("usage.pin.backToAuto")}</button></p></div>}
        {resting && <div className="tiger-out" role="status">
          <img src={tigerStrip} alt="" width={56} height={56} />
          <div><strong>{t("usage.tigerOut.title", { provider: providerNames[provider] })}</strong><p>{resting.until ? t("usage.tigerOut.until", { time: relativeTime(resting.until, true) }) : t("usage.tigerOut.unknown")}</p></div>
        </div>}

        <div className="acc-list">{providerRows.map(row => {
          const member = row.group && pinMember(row.group, provider);
          return <AccountRow key={row.key} row={row} provider={provider} masked={masked} online={online} now={now} reserve={reserve} open={isExpanded(open, rowIds(row))}
            onToggle={() => setOpen(current => toggleExpanded(current, rowIds(row)))} tools={snapshot.tools} bridgeOn={bridgeOn} usageUnavailable={usageUnavailable} usageError={error}
            periodLabel={periodLabel} peak={peak} manual={manual} pinned={pinnedGroup(provider, row)} canPin={Boolean(member) && !row.unknown} pinPending={pinAction.pending}
            onPin={() => { if (member) setPin(provider, member.id); }} expiringBoosted={Boolean(snapshot.policy.expiringBoost) && (snapshot.policy.allocationMode ?? "smart") === "smart"}
            refreshing={refreshing} onRefresh={onRefresh} onConnect={onConnect} accounts={snapshot.accounts} onReload={onReload} watch={watch} />;
        })}</div>
      </section>;
    })}
    {bridgeOn && <details className="panel history">
      <summary><h2>{t("usage.history.title")}</h2><span className="aside">{t("usage.history.summary", { period: periodLabel })}</span></summary>
    {bridgeOn && <section className={`now${!usageUnavailable && !lead && loaded ? " quiet" : ""}`} aria-label={t("usage.busiestAria")}>
      <div className="busiest">
        <div className="who"><span className={!usageUnavailable && lead && lead.cell.now ? "live" : "dot idle"} />{t("usage.busiest")}</div>
        {usageUnavailable ? <><div className="model muted">{t(error ? "usage.readFailed" : "common.checking")}</div><div className="rate">{t(error ? "usage.readFailedHelp" : "usage.loadingHelp")}</div></> : lead ? <>
          <div className="model">{lead.cell.name}</div>
          <div className="who"><Dot provider={lead.row.provider} />{providerNames[lead.row.provider]}, {accountText(lead.row.account, masked)}</div>
          <div className="rate">{t("usage.last5min")}<b>{t("usage.requestCount", { count: lead.cell.now })}</b>, {t("usage.periodTotal", { period: periodLabel, count: t("usage.requestCount", { count: lead.cell.total }) })}</div>
          <Sparkline series={lead.cell.series} peak={peak} provider={lead.row.provider} width={300} height={44} />
        </> : loaded ? <div className={characters ? "idle-scene" : undefined}>
          {characters && <img className="idle-art" src={tigerNap} alt="" width={140} height={68} />}
          <div><div className="model muted">{t("usage.noRecentCalls")}</div><div className="rate">{t("usage.noRequests", { period: periodLabel })}</div></div>
        </div> : <><div className="model muted">{t("common.checking")}</div><div className="rate">{t("usage.noRequests", { period: periodLabel })}</div></>}
      </div>
      <ol className="ranking">{lines.slice(1, 7).map((line, index) => <li key={`${line.row.key}:${line.cell.key}`}>
        <span className="rank">{index + 2}</span>
        <span className="line">{line.cell.now > 0 && <span className="live inline" />}{line.cell.name} <span>{providerNames[line.row.provider]}, {accountText(line.row.account, masked)}</span></span>
        <Sparkline series={line.cell.series} peak={peak} provider={line.row.provider} />
        <span className="count">{t("usage.countShort", { count: line.cell.total })}</span>
      </li>)}</ol>
    </section>}
    {bridgeOn && routeDetails.length > 0 && <section className="panel route-details" aria-label={t("usage.routes.title")}>
      <header><h2>{t("usage.routes.title")}</h2><span className="aside">{periodLabel} · omp</span></header>
      <div className="route-table-scroll"><table>
        <thead><tr><th scope="col">{t("usage.routes.project")}</th><th scope="col">{t("usage.routes.model")}</th><th scope="col">{t("usage.routes.status")}</th><th scope="col">{t("usage.routes.lastRecorded")}</th></tr></thead>
        <tbody>{routeDetails.map(detail => <tr key={detail.key}>
          <td title={!masked && detail.project ? detail.project : undefined}>{detail.project ? projectName(detail.project, masked) : t("usage.routes.projectUnknown")}</td>
          <td><strong>{detail.model}</strong><div className="muted">{providerNames[detail.provider] ?? detail.provider}</div></td>
          <td><Badge tone={detail.route === "direct" ? "direct" : "neutral"}>{t(detail.route === "direct" ? "usage.routes.direct" : "usage.routes.unknown")}</Badge></td>
          <td><time dateTime={new Date(detail.lastRecordedAt).toISOString()}>{fullTime(detail.lastRecordedAt)}</time></td>
        </tr>)}</tbody>
      </table></div>
    </section>}
    </details>}
    <p className="timezone-note">{t("usage.timezone", { zone: timeZoneLabel() })}</p>
    {!bridgeOn && <div className="notice"><p><b>{t("usage.bridgeDisconnected.title")}</b> {t("usage.bridgeDisconnected.body")} <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>}
  </section>;
}
