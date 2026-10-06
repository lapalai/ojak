import { useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { ActionFeedback, Badge, Dot, ErrorMessage, Sparkline } from "./components";
import { AllocationSettings } from "./AllocationSettings";
import { rpc } from "./api";
import { absoluteTime, bucketState, canonicalProvider, groupAccounts, isCurrentGroup, projectName, providerColor, providerNames, relativeTime, toolNames, useAction, useBridge, useCharacters } from "./state";
import type { AccountGroup } from "./state";
import { PROVIDER_ORDER } from "./types";
import type { Account, AccountQuotaSummary as Verdict, Policy, QuotaBucket, Snapshot } from "./types";
import { limitLabel, t } from "./i18n";
import { observedRouteModels } from "./usage-routes";
import { allResting, remainingTone } from "./limits";
import tigerStrip from "./assets/tiger-strip.webp";
import tigerNap from "./assets/tiger-nap.webp";

type Period = "15" | "60" | "today";
const periods: Period[] = ["15", "60", "today"];
/// 서비스가 5시간 한도 버킷 라벨 끝에 붙이는 표시(crates/adapters/src/quota.rs). 서버 문자열과 맞추는 용도라 번역하지 않는다.

interface ModelCell { key: string; name: string; series: number[]; total: number; now: number; projects: string[]; route: "bridge" | "direct" | "unknown"; roles: string[] }
interface Quota { label: string; value: number | null; state: string; bucket: QuotaBucket }
interface UsageRow { key: string; provider: string; account: string | null; group: AccountGroup | null; cells: ModelCell[]; quotas: Quota[]; verdict: Verdict | null; detail: string[]; notes: string[]; off: boolean; unknown: boolean }

const verdictTone: Record<Verdict["kind"], string> = { available: "good", partial: "warning", reserve: "warning", resting: "danger", excluded: "neutral", login: "danger", unknown: "neutral" };

/// 카드 맨 위 배지 문구: 결론과, 풀리는 때가 있으면 그 시각.
function verdictText(verdict: Verdict): string {
  const head = verdict.kind === "partial" ? t("usage.verdict.partial", { models: verdict.models.join(", ") }) : t(`usage.verdict.${verdict.kind}`);
  return verdict.until ? `${head} · ${t("usage.verdict.back", { time: relativeTime(verdict.until, true) })}` : head;
}

/// 배지 아래 한 줄 이유. 사용 가능이면 없다.
function verdictReason(verdict: Verdict, reserve: number): string | null {
  const label = verdict.label ? limitLabel(verdict.label.split(" · ").pop() ?? verdict.label) : "";
  if (verdict.kind === "resting") return verdict.rate ? t("usage.reason.rate") : t("usage.reason.resting", { label });
  if (verdict.kind === "reserve") return t("usage.reason.reserve", { label, reserve });
  if (verdict.kind === "partial") return t("usage.reason.partial", { models: verdict.models.join(", ") });
  if (verdict.kind === "login") return t("usage.reason.login");
  if (verdict.kind === "unknown") return t("usage.reason.unknown");
  return null;
}

function roleLabel(role: string): string {
  return role === "main" ? t("usage.role.main") : role === "subagent" ? t("usage.role.subagent") : role === "auxiliary" ? t("usage.role.auxiliary") : t("usage.role.unknown");
}

/// 이메일이나 `account:<id>` 형태의 계정 문자열. 가림 모드에서는 종류만 남긴다.
function accountText(account: string | null, masked: boolean): string {
  if (!account) return t("account.unknown");
  if (masked) return account.includes("@") ? t("privacy.emailHidden") : t("privacy.accountHidden");
  return account;
}

/// 공급자 버킷 라벨(`Claude 5 Hour · 5시간`, `Gemini · 주간`, `Fable 주간`)을 짧은 표시 이름으로 줄인다.
/// 같은 계정 안에서 짧은 이름이 겹치는데 값까지 같으면 같은 한도를 두 도구가 본 것이므로 하나로 합치고,
/// 값이 다르면(Antigravity의 Gemini·Claude & GPT처럼) 앞부분을 붙여 구분한다.
function quotaRows(buckets: QuotaBucket[], staleAfter: number): Quota[] {
  const parts = buckets.map(bucket => bucket.label.split(" · "));
  const shorts = parts.map(segments => segments[segments.length - 1]);
  const rows = new Map<string, Quota>();
  buckets.forEach((bucket, index) => {
    const siblings = buckets.filter((_, otherIndex) => otherIndex !== index && shorts[otherIndex] === shorts[index]);
    const sameValue = siblings.every(other => other.usedPercent !== null && bucket.usedPercent !== null && Math.abs(other.usedPercent - bucket.usedPercent) < 0.5);
    const duplicate = siblings.length > 0 && !sameValue && parts[index].length > 1;
    const label = duplicate ? `${parts[index][0]} ${shorts[index]}` : shorts[index];
    const state = bucketState(bucket, staleAfter);
    const value = bucket.usedPercent !== null && Number.isFinite(bucket.usedPercent) ? bucket.usedPercent : null;
    const existing = rows.get(label);
    if (!existing || (value ?? -1) > (existing.value ?? -1)) rows.set(label, { label, value, state, bucket });
  });
  return [...rows.values()];
}

function formatPercent(value: number): string {
  return value >= 10 ? String(Math.round(value)) : String(Math.round(value * 10) / 10);
}

/// 수동 배정 키는 화면의 공급자 묶음 이름(서비스 `pin_provider`와 같은 규칙)이다.
/// 그룹 안에서 이 공급자 키로 고정할 계정. 서비스가 계정의 공급자와 키를 대조하므로 맞는 구성원을 고른다.
function pinMember(group: AccountGroup, provider: string): Account | undefined {
  return group.members.find(member => canonicalProvider(member.provider) === provider) ?? undefined;
}

export function UsageView({ snapshot, masked, online, onReload, onConnect, onSessions }: { snapshot: Snapshot; masked: boolean; online: boolean; onReload: () => Promise<void>; onConnect: () => void; onSessions: () => void }) {
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
      key: group.key, provider: canonicalProvider(group.primary.provider), account: group.email, group, cells: [], quotas: quotaRows(group.buckets, staleAfter), verdict: null, detail: [], notes: [], off: group.members.every(member => !member.enabled), unknown: false,
    }));
    const find = (provider: string, account: string) => rows.find(row => row.provider === provider && (row.group ? row.group.keys.includes(account.toLowerCase()) : row.account?.toLowerCase() === account.toLowerCase()));
    const synthesize = (provider: string, account: string): UsageRow => {
      const row: UsageRow = { key: `${provider}:${account}`, provider, account, group: null, cells: [], quotas: [], verdict: null, detail: [], notes: [t("usage.note.unregisteredBridgeAccount")], off: false, unknown: false };
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
        const created: UsageRow = { key, provider: model.provider, account: null, group: null, cells: [], quotas: [], verdict: null, detail: [], notes: [t(model.route === "direct" ? "usage.note.directCall" : "usage.note.routeUnknown")], off: false, unknown: true };
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
      row.detail = blocks.map(block => `${block.scope ?? "*"} · ${block.reason} · ${absoluteTime(block.until)}`);
      if (row.group && !row.unknown) {
        row.verdict = snapshot.quotaSummaries?.find(summary => summary.accountIds.includes(row.group!.primary.id))
          ?? { accountIds: row.group.members.map(member => member.id), kind: "unknown", until: null, models: [], label: null, rate: false };
        if (!snapshot.quotaSummaries) row.notes.push(t("api.protocolMismatch"));
      }
    }
    rows.sort((a, b) => Number(a.unknown) - Number(b.unknown) || b.cells.reduce((sum, cell) => sum + cell.total, 0) - a.cells.reduce((sum, cell) => sum + cell.total, 0) || (a.account ?? "").localeCompare(b.account ?? ""));
    const peak = Math.max(1, ...rows.flatMap(row => row.cells.flatMap(cell => cell.series)));
    const routeDetails = observed.flatMap(model => model.observations.map(observation => ({
      ...observation, key: JSON.stringify([model.key, observation.project]), provider: model.provider, model: model.model, route: model.route,
    }))).sort((a, b) => b.lastRecordedAt - a.lastRecordedAt || a.key.localeCompare(b.key));
    return { rows, peak, directCount: observed.filter(model => model.route === "direct").length, unknownCount: observed.filter(model => model.route === "unknown").length, routeDetails };
  }, [snapshot.accounts, snapshot.quotaSummaries, snapshot.observedSessions, bridge, usage, windowMinutes, reserve, staleAfter]);

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
    <div className="notice"><p>{bridgeOn ? t("usage.scope") : t("usage.scope.accounts")} <button type="button" className="text-button" onClick={onSessions}>{t("nav.sessions")}</button> · <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>
    <details className="settings-strip">
      <summary>{t("usage.settings.summary", { mode: (snapshot.policy.allocationMode ?? "smart") === "priority" ? t("policy.mode.priority") : t("policy.mode.smart"), reserve })}</summary>
      <AllocationSettings snapshot={snapshot} masked={masked} online={online} onReload={onReload} />
    </details>
    <ActionFeedback error={pinAction.error} message={pinAction.message} />
    {error && bridgeOn && <ErrorMessage error={error} />}
    {bridgeOn && directCount > 0 && <div className="notice" style={{ "--tone": "var(--warning)" } as CSSProperties}><p><b>{t("usage.direct.title", { count: directCount })}</b> {t("usage.direct.bodyBefore")}<code>/model</code>{t("usage.direct.bodyAfter")}</p></div>}
    {bridgeOn && unknownCount > 0 && <div className="notice"><p><b>{t("usage.routeUnknown.title", { count: unknownCount })}</b> {t("usage.routeUnknown.body")} <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>}
    {bridgeOn && routeDetails.length > 0 && <section className="panel route-details" aria-label={t("usage.routes.title")}>
      <header><h2>{t("usage.routes.title")}</h2><span className="aside">{periodLabel} · omp</span></header>
      <div className="route-table-scroll"><table>
        <thead><tr><th scope="col">{t("usage.routes.project")}</th><th scope="col">{t("usage.routes.model")}</th><th scope="col">{t("usage.routes.status")}</th><th scope="col">{t("usage.routes.lastRecorded")}</th></tr></thead>
        <tbody>{routeDetails.map(detail => <tr key={detail.key}>
          <td title={!masked && detail.project ? detail.project : undefined}>{detail.project ? projectName(detail.project, masked) : t("usage.routes.projectUnknown")}</td>
          <td><strong>{detail.model}</strong><div className="muted">{providerNames[detail.provider] ?? detail.provider}</div></td>
          <td><Badge tone={detail.route === "direct" ? "direct" : "neutral"}>{t(detail.route === "direct" ? "usage.routes.direct" : "usage.routes.unknown")}</Badge></td>
          <td><time dateTime={new Date(detail.lastRecordedAt).toISOString()}>{absoluteTime(detail.lastRecordedAt)}</time></td>
        </tr>)}</tbody>
      </table></div>
    </section>}
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

        {providerRows.map(row => <div className={`acct ${row.off ? "off" : ""} ${bridgeOn ? "" : "quota-only"}`} key={row.key}>
          <div className="acct-info">
            <div className="who">
              {manual && row.group && pinMember(row.group, provider) && <input type="radio" name={`pin-${provider}`} className="pin-radio" checked={pinnedGroup(provider, row)} disabled={!online || pinAction.pending} aria-label={t("usage.pin.choose", { account: accountText(row.account, masked) })} onChange={() => { const member = row.group && pinMember(row.group, provider); if (member) setPin(provider, member.id); }} />}
              {row.unknown ? <Badge tone={row.cells.some(cell => cell.route === "direct") ? "direct" : "neutral"}>{t("account.unknown")}</Badge> : accountText(row.account, masked)}
              {row.group && row.group.members.map(member => member.tool).filter((tool, index, list) => tool !== "omp" && list.indexOf(tool) === index).map(tool => <span className="tag" key={tool}>{toolNames[tool] || tool}</span>)}
              {manual && pinnedGroup(provider, row) && <Badge tone="good">{t("usage.pin.current")}</Badge>}
              {!manual && row.group && pinMember(row.group, provider) && <button type="button" className="text-button pin-button" disabled={!online || pinAction.pending} onClick={() => { const member = row.group && pinMember(row.group, provider); if (member) setPin(provider, member.id); }}>{t("usage.pin.fix")}</button>}
            </div>
            {row.verdict && <div className={`verdict ${verdictTone[row.verdict.kind]}`} title={row.detail.join("\n") || undefined}><span className="dot" />{verdictText(row.verdict)}</div>}
            {row.verdict?.expiring && <div className="expiring-hint" title={t("usage.expiring.title", { label: limitLabel(row.verdict.expiring.label), reset: absoluteTime(row.verdict.expiring.resetsAt) })}>
              <span className="dot" />{t("usage.expiring.line", { reset: relativeTime(row.verdict.expiring.resetsAt, true), percent: formatPercent(row.verdict.expiring.usablePercent) })}
              {snapshot.policy.expiringBoost && (snapshot.policy.allocationMode ?? "smart") === "smart" && <small>{t("usage.expiring.boosted")}</small>}
            </div>}
            {row.verdict && verdictReason(row.verdict, reserve) && <div className="reason">{verdictReason(row.verdict, reserve)}</div>}
            {row.notes.filter(note => !(row.verdict?.kind === "excluded" && note === t("allocation.excluded"))).map(note => <div className="note" key={note}>{note}</div>)}
            {row.quotas.length > 0 && <div className="quota">{row.quotas.map(quota => {
              // 메뉴바와 같게 남은 양으로 보여 준다. 소진은 막대 대신 다시 쓸 수 있는 때를 적는다.
              const spent = quota.state === "exhausted" || (quota.value !== null && quota.value >= 100);
              const left = quota.value === null ? null : Math.max(0, 100 - quota.value);
              // 색은 남은 양만 뜻한다(메뉴바와 같은 기준). 막대에 공급자 색을 쓰면 100%도 공급자마다 달라 보인다.
              const tone = spent ? "bad" : remainingTone(left, reserve);
              const weeklyReset = quota.bucket.label.includes("주간") && quota.bucket.resetsAt !== null
                && Number.isFinite(quota.bucket.resetsAt) && quota.bucket.resetsAt > 0 ? quota.bucket.resetsAt : null;
              return <div className={`q ${tone} ${quota.bucket.model ? "sub" : ""} ${quota.state === "stale" || quota.state === "unknown" ? "stale" : ""}`} key={quota.label} title={t("usage.quotaTitle", { label: limitLabel(quota.bucket.label), observed: absoluteTime(quota.bucket.observedAt), reset: absoluteTime(quota.bucket.resetsAt) })}>
                <span>{limitLabel(quota.label)}</span>
                <div className="bar"><i style={{ width: `${left ?? 0}%` }} /></div>
                <b>{spent ? (weeklyReset === null && quota.bucket.resetsAt ? `${t("usage.quota.spent")} · ${relativeTime(quota.bucket.resetsAt, true)}` : t("usage.quota.spent")) : left === null ? "—" : t("usage.quota.left", { value: formatPercent(left) })}</b>
                {weeklyReset !== null && <small className="quota-reset" title={absoluteTime(weeklyReset)}>
                  {weeklyReset <= Date.now() ? t("time.resetPending") : t("usage.quota.reset", { time: relativeTime(weeklyReset, true) })}
                </small>}
              </div>;
            })}</div>}
          </div>
          {bridgeOn && (row.cells.length ? <div className="models">{row.cells.map(cell => <div className="cell" key={cell.key} style={{ "--c": providerColor[provider], "--heat": Math.min(1, cell.total / peak).toFixed(2) } as CSSProperties}>
            <div className="head">{cell.now > 0 && <span className="live" />}<span className="name">{cell.name}</span><span className="n">{cell.route !== "bridge" ? <small>{t("usage.countUnknown")}</small> : <>{cell.total}<small>{t("usage.countUnit")}</small></>}</span></div>
            {cell.route !== "bridge" ? <div className="cell-gap" /> : <Sparkline series={cell.series} peak={peak} provider={provider} width={220} height={26} />}
            <div className="where">{masked ? cell.projects.length > 0 && <span className="tag">{t("usage.projectsHidden", { count: cell.projects.length })}</span> : cell.projects.map(cwd => <span className="tag" key={cwd} title={cwd}>{projectName(cwd, masked)}</span>)}{cell.route !== "bridge" && <span className={`tag ${cell.route === "direct" ? "direct" : "neutral"}`}>{t(cell.route === "direct" ? "usage.direct.tag" : "usage.routeUnknown.tag")}</span>}{cell.roles.map(role => <span className="tag" key={role}>{role}</span>)}</div>
          </div>)}</div> : <div className="empty">{usageUnavailable ? t(error ? "usage.readFailed" : "common.checking") : t("usage.noCalls", { period: periodLabel })}</div>)}
        </div>)}
      </section>;
    })}
    {!bridgeOn && <div className="notice"><p><b>{t("usage.bridgeDisconnected.title")}</b> {t("usage.bridgeDisconnected.body")} <button type="button" className="text-button" onClick={onConnect}>{t("nav.connect")}</button></p></div>}
  </section>;
}
