import { useRef, useState } from "react";
import type { CSSProperties } from "react";
import { Bell, BellRing, LogIn, RefreshCw } from "lucide-react";
import { ActionFeedback, Busy, Sparkline } from "./components";
import { ProviderLoginDialog } from "./LoginDialog";
import { ResetCreditsPanel } from "./ResetCreditsPanel";
import { reloginRequest } from "./login";
import { fullTime, privacyText, projectName, providerColor, relativeTime, shortTime, toolNames, useAction } from "./state";
import { limitLabel, t } from "./i18n";
import { creditCount, quotaReading, usdLimit, usdUsed } from "./limits";
import { overviewOf, qualifiedLabel, quotaTone, watchable, watchedMembers } from "./overview";
import type { Overview, UsageRow, When } from "./overview";
import type { Account, AccountQuotaSummary as Verdict, ApiError, ToolStatus } from "./types";
import type { AccountWork } from "./usage-activity";

export function formatPercent(value: number): string {
  return value >= 10 ? String(Math.round(value)) : String(Math.round(value * 10) / 10);
}

/// 이메일이나 `account:<id>` 형태의 계정 문자열. 가림 모드에서는 종류만 남긴다.
export function accountText(account: string | null, masked: boolean): string {
  if (!account) return t("account.unknown");
  if (masked) return account.includes("@") ? t("privacy.emailHidden") : t("privacy.accountHidden");
  return account;
}

export function accountName(row: UsageRow, masked: boolean): string {
  return row.unknown ? t("account.unknown") : row.alias ? privacyText(row.alias, masked) : row.account ? accountText(row.account, masked) : row.group ? privacyText(row.group.primary.label, masked) : t("account.unknown");
}

/// 크레딧(Codex)·추가 사용량(Claude, USD) 한 줄. 두 표기는 단위가 달라 섞지 않는다. 쓰는 중이 아니면 옵션이 있다는 중립 안내만 한다.
function paidLine(verdict: Verdict): string | null {
  const { credits, extraUsage: extra } = verdict;
  if (extra) {
    if (extra.active) return extra.limitUsd !== null ? t("usage.extra.amount", { used: usdUsed(extra.usedUsd), limit: usdLimit(extra.limitUsd) }) : t("usage.extra.amountOpen", { used: usdUsed(extra.usedUsd) });
    return extra.limitReached ? t("usage.extra.capped") : t("usage.extra.hint");
  }
  if (credits) {
    if (credits.active) {
      const count = creditCount(credits.balance);
      return credits.unlimited ? t("usage.credits.unlimited") : count !== null ? t("usage.credits.balance", { count }) : null;
    }
    return t("usage.credits.hint");
  }
  return null;
}
function paidTitle(verdict: Verdict): string | null {
  if (verdict.extraUsage && !verdict.extraUsage.active && !verdict.extraUsage.limitReached) return t("usage.extra.hintTitle");
  if (!verdict.extraUsage && verdict.credits && !verdict.credits.active) return t("usage.credits.hintTitle");
  return null;
}

/// 한 줄 이유. 사용 가능이면 없다. 어느 경우에도 새 작업이 이 계정으로 간다는 약속은 하지 않는다.
function verdictReason(verdict: Verdict, reserve: number): string | null {
  const label = verdict.label ? limitLabel(verdict.label.split(" · ").pop() ?? verdict.label) : "";
  if (verdict.kind === "credits") return t("usage.reason.credits");
  if (verdict.kind === "extra") return t("usage.reason.extra");
  if (verdict.kind === "resting") return verdict.rate ? t("usage.reason.rate") : label ? t("usage.reason.resting", { label }) : t("usage.reason.restingGeneric");
  if (verdict.kind === "reserve") return t("usage.reason.reserve", { label, reserve });
  if (verdict.kind === "partial") return t("usage.reason.partial", { models: verdict.models.join(", ") });
  if (verdict.kind === "login") return t("usage.reason.login");
  if (verdict.kind === "unknown") return t("usage.reason.unknown");
  return null;
}

/// 접힌 줄의 상태 문구. 쉬는 중이면 무엇이 막았는지(주간·5시간·요청 속도·그 밖의 한도), 일부 모델 제한이면 모델 이름을 말한다.
function stateLabel(overview: Overview, verdict: Verdict): string {
  const { blocker } = overview;
  if (overview.state === "availableLow") return t("usage.state.availableLow");
  if (overview.state === "partial") return t("usage.verdict.partial", { models: overview.models.join(", ") });
  if (overview.state === "resting" && blocker) {
    return blocker.kind === "weekly" ? t("usage.state.restingWeekly") : blocker.kind === "fiveHour" ? t("usage.state.restingFiveHour")
      : blocker.kind === "rate" ? t("usage.state.restingRate") : blocker.kind === "limit" ? t("usage.state.restingLimit", { label: limitLabel(blocker.label) }) : t("usage.verdict.resting");
  }
  return t(`usage.verdict.${verdict.kind}`);
}

function whenText(when: When): string {
  if (when.kind === "expiring") return t("usage.when.expiring", { time: shortTime(when.at), percent: formatPercent(when.percent) });
  if (when.at === null) return t("usage.when.unknown");
  const time = shortTime(when.at);
  return when.kind === "recovery" ? t("usage.verdict.back", { time }) : when.kind === "modelBack" ? t("usage.when.modelReset", { time }) : t("usage.quota.reset", { time });
}

export interface WatchControl {
  /// 이 환경에서 알림 감시를 쓸 수 있는지(데스크톱 앱).
  available: boolean;
  watched: readonly string[] | null;
  set: (accountId: string, enabled: boolean) => Promise<string[]>;
}

export interface AccountDetailsProps {
  row: UsageRow;
  provider: string;
  masked: boolean;
  online: boolean;
  now: number;
  reserve: number;
  work: AccountWork[];
  tools: ToolStatus[];
  bridgeOn: boolean;
  usageUnavailable: boolean;
  usageError: ApiError | null;
  periodLabel: string;
  peak: number;
  pinned: boolean;
  canPin: boolean;
  pinPending: boolean;
  onChoose: () => void;
  expiringBoosted: boolean;
  refreshing: boolean;
  onRefresh: () => void;
  onConnect: () => void;
  accounts: readonly Account[];
  onReload: () => Promise<void>;
  watch: WatchControl;
}

export interface AccountRowProps extends AccountDetailsProps {
  selected: boolean;
  onSelect: (trigger: HTMLButtonElement) => void;
}

function sharedProfileCount(row: UsageRow): number {
  const members = row.group?.members ?? [];
  return members.filter(member => member.identityKey && member.profilePath && members.some(other =>
    other.id !== member.id && other.tool === member.tool && other.identityKey === member.identityKey && other.profilePath !== member.profilePath)).length;
}

export function AccountRow(props: AccountRowProps) {
  const { row, masked, online, now, reserve, selected } = props;
  const button = useRef<HTMLButtonElement>(null);
  const overview = row.verdict ? overviewOf(row.verdict, row.quotas, now) : null;
  const name = accountName(row, masked);
  const toolTags = [...new Set(row.group?.members.map(member => toolNames[member.tool] || member.tool))];
  const sharedProfiles = sharedProfileCount(row);
  const reason = row.verdict ? verdictReason(row.verdict, reserve) : null;
  const status = !online ? t("usage.dashboard.offline") : overview && row.verdict ? stateLabel(overview, row.verdict) : row.notes[0] ?? t("account.unknown");
  const common = row.quotas.flatMap(quota => {
    const reading = quotaReading(quota.bucket, quota.state);
    return !quota.bucket.model && reading.confirmed ? [{ quota, reading }] : [];
  }).sort((a, b) => Number(b.quota.label === overview?.limit?.label) - Number(a.quota.label === overview?.limit?.label)).slice(0, 2);
  const modelLimit = overview?.modelLimit;
  return <div className={`acc${selected ? " selected" : ""}${row.off ? " off" : ""}`} data-tone={overview?.tone ?? "neutral"}>
    <div className="acc-head" onClick={event => { if (!(event.target as HTMLElement).closest("button, input, a, label") && button.current) props.onSelect(button.current); }}>
      <button ref={button} type="button" className="acc-name" aria-expanded={selected} aria-controls={selected ? "usage-account-detail" : undefined}
        aria-label={t("usage.row.show", { account: name })} onClick={event => props.onSelect(event.currentTarget)}>
        <span className="acc-id"><strong title={masked ? undefined : name}>{name}</strong>
          <small>{toolTags.join(" · ")}{props.pinned && <span className="pin-mark"> · {t("usage.pin.pinned")}</span>}{props.work.length > 0 && <span className="acc-work"> · {t("usage.dashboard.workCount", { count: props.work.length })}</span>}</small>
          {sharedProfiles > 1 && <small title={t("account.sharedQuota", { count: sharedProfiles })}>{t("account.sharedProfiles")}</small>}
        </span>
      </button>
      <div className={`acc-state ${online ? overview?.tone ?? "neutral" : "neutral"}`} title={reason ?? undefined}><span className="dot" aria-hidden="true" /><span className="acc-state-text">{status}</span></div>
      <div className="acc-limit">{common.length ? common.map(({ quota, reading }) => {
        const tone = quotaTone(reading, overview?.blockedBy ?? null, reserve);
        return <div className={`acc-mini-quota ${tone}`} key={quota.label}>
          <div className="lim"><span className="lim-label">{limitLabel(quota.label)}</span><b>{reading.spent ? t("usage.quota.spent") : t("usage.quota.left", { value: formatPercent(reading.left ?? 0) })}</b></div>
          <div className={`bar mini ${tone}`} aria-hidden="true"><i style={{ width: `${reading.left ?? 0}%` }} /></div>
        </div>;
      }) : !modelLimit && <span className="muted">{t("usage.limit.none")}</span>}
        {modelLimit && <small className="lim-model">{limitLabel(qualifiedLabel(modelLimit.label, modelLimit.model))} · {modelLimit.spent ? t("usage.quota.spent") : t("usage.quota.left", { value: formatPercent(modelLimit.left ?? 0) })}</small>}
      </div>
      <div className="acc-when">{overview?.when ? overview.when.at !== null
        ? <time dateTime={new Date(overview.when.at).toISOString()} title={fullTime(overview.when.at)}>{whenText(overview.when)}</time>
        : <span className="muted">{whenText(overview.when)}</span> : <span className="muted">—</span>}</div>
      <AccountActions {...props} overview={overview} compact />
    </div>
  </div>;
}

function AccountActions(props: AccountDetailsProps & { overview: Overview | null; compact?: boolean }) {
  const { row, online, tools, watch, overview, compact } = props;
  const watchAction = useAction();
  const [loginOpen, setLoginOpen] = useState(false);
  const group = row.group;
  const name = accountName(row, props.masked);
  const memberIds = group ? group.members.map(member => member.id) : [];
  const watchedIds = watchedMembers(watch.watched, memberIds);
  const watching = watchedIds.length > 0;
  const showWatch = watch.available && group !== null && overview !== null && (watchable(overview.state) || watching);
  const login = group && overview?.state === "login" ? reloginRequest(group.members, tool => tools.some(item => item.id === tool && item.installed)) : null;
  const ompLogin = overview?.state === "login" && group?.members.some(member => member.tool === "omp" && member.authStatus === "auth-required");
  const toggleWatch = () => {
    if (!group) return;
    const target = watching ? watchedIds[0] : group.primary.id;
    void watchAction.run(async () => {
      const next = await watch.set(target, !watching);
      if (!watching && watchedMembers(next, memberIds).length === 0) throw { code: "REQUEST_FAILED", message: t("api.requestFailed"), retryable: true } satisfies ApiError;
      return !watching;
    }, enabled => enabled ? t("usage.watch.saved") : t("usage.watch.cancelled"));
  };
  return <>
    <div className={compact ? "acc-actions" : "detail-actions"}>
      {login ? <button type="button" className={compact ? "icon-button" : undefined} disabled={!online} title={t("account.relogin")} aria-label={t("account.reloginAria", { name })} onClick={() => setLoginOpen(true)}>{compact ? <LogIn size={16} /> : t("account.relogin")}</button>
        : overview?.state === "login" ? <button type="button" className={compact ? "icon-button" : undefined} title={t("nav.connect")} aria-label={t("nav.connect")} onClick={props.onConnect}>{compact ? <LogIn size={16} /> : t("nav.connect")}</button> : null}
      {overview?.state === "unknown" && <button type="button" className="icon-button" disabled={!online || props.refreshing} onClick={props.onRefresh} title={t("common.recheck")} aria-label={t("common.recheck")}><RefreshCw size={14} className={props.refreshing ? "spinner" : undefined} /></button>}
      {showWatch && <button type="button" className={`${compact ? "icon-button " : ""}watch${watching ? " on" : ""}`} aria-pressed={watching} disabled={watchAction.pending || (!watching && !online)}
        aria-label={t(watching ? "usage.watch.disableAria" : "usage.watch.enableAria", { account: name })} title={t(watching ? "usage.watch.disableTitle" : "usage.watch.enableTitle")} onClick={toggleWatch}>
        {watchAction.pending ? <Busy label={compact ? "" : t("common.checking")} /> : <>{watching ? <BellRing size={14} /> : <Bell size={14} />}{!compact && t(watching ? "usage.watch.enabled" : "usage.watch.enable")}</>}
      </button>}
    </div>
    {ompLogin && login && <p className="acc-muted-note">{t("account.ompLoginExpired")}</p>}
    {loginOpen && login && <ProviderLoginDialog request={login} accounts={props.accounts} online={online} onReload={props.onReload} onClose={() => setLoginOpen(false)} />}
    {(watchAction.error || watchAction.message) && <div className="acc-feedback"><ActionFeedback error={watchAction.error} message={watchAction.message} /></div>}
  </>;
}

export function AccountDetails(props: AccountDetailsProps) {
  const { row, provider, masked, online, now, reserve, canPin, pinPending } = props;
  const verdict = row.verdict;
  const overview = verdict ? overviewOf(verdict, row.quotas, now) : null;
  const group = row.group;
  const name = accountName(row, masked);
  const reason = verdict ? verdictReason(verdict, reserve) : null;
  const ompLogin = overview?.state === "login" && group?.members.some(member => member.tool === "omp" && member.authStatus === "auth-required");
  const sharedProfiles = sharedProfileCount(row);
  return <div className="acc-detail">
    {overview && verdict && <p className={`detail-status ${online ? overview.tone : "neutral"}`}><span className="dot" />{online ? stateLabel(overview, verdict) : t("usage.dashboard.offline")}</p>}
    <AccountActions {...props} overview={overview} />
    <section aria-label={t("usage.dashboard.currentWork")}>
      <h4>{t("usage.dashboard.currentWork")}</h4>
      {props.work.length ? <ul className="current-work">{props.work.map(work => <li key={work.id}>
        <strong>{toolNames[work.tool] || work.tool}</strong>
        <span>{work.model ? t(work.modelSource === "request" ? "usage.dashboard.requestedModel" : "usage.dashboard.observedModel", { model: work.model === "native-default" ? t("session.defaultModel") : work.model }) : t("usage.dashboard.modelUnconfirmed")}</span>
        <small title={masked ? undefined : work.cwd ?? undefined}>{work.cwd ? projectName(work.cwd, masked) : t("usage.routes.projectUnknown")}</small>
      </li>)}</ul> : <p className="sub">{t(online ? "usage.dashboard.noCurrentWork" : "usage.dashboard.offline")}</p>}
    </section>
      {overview && overview.blockedBy !== null && <p className="acc-muted-note">{t(overview.blockedBy === "quota" ? "usage.detail.mutedQuota" : "usage.detail.mutedAccount")}</p>}
      <div className="acc-cols">
        <section aria-label={t("usage.detail.account")}>
          <h4>{t("usage.detail.account")}</h4>
          <dl className="acc-dl">
            {row.alias && <><dt>{t("usage.detail.name")}</dt><dd>{privacyText(row.alias, masked)}</dd></>}
            <dt>{t("usage.detail.email")}</dt><dd>{accountText(row.account, masked)}</dd>
            {group && <><dt>{t("usage.detail.tools")}</dt><dd>{group.members.map(member => `${toolNames[member.tool] || member.tool}${member.plan ? ` (${member.plan})` : ""}`).join(", ")}</dd></>}
            {sharedProfiles > 1 && <><dt>{t("account.sharedProfiles")}</dt><dd>{t("account.sharedQuota", { count: sharedProfiles })}</dd></>}
          </dl>
          {reason && !ompLogin && <p className="reason">{reason}</p>}
          {verdict && paidLine(verdict) && <p className="paid-hint" title={paidTitle(verdict) ?? undefined}>{paidLine(verdict)}</p>}
          {verdict?.expiring && <p className="expiring-hint" title={t("usage.expiring.title", { label: limitLabel(verdict.expiring.label), reset: fullTime(verdict.expiring.resetsAt) })}>
            <span className="dot" />{t("usage.expiring.line", { reset: relativeTime(verdict.expiring.resetsAt, true), percent: formatPercent(verdict.expiring.usablePercent) })}
            {props.expiringBoosted && <small>{t("usage.expiring.boosted")}</small>}
          </p>}
          {row.notes.filter(note => !(verdict?.kind === "excluded" && note === t("allocation.excluded"))).map(note => <p className="note" key={note}>{note}</p>)}
          {group && canPin && <button type="button" className="text-button pin-button" disabled={!online || pinPending} onClick={props.onChoose}>{t("usage.dashboard.choose")}</button>}
          {group && <ResetCreditsPanel key={group.primary.id} accountId={group.primary.id} name={name} online={online} onReload={props.onReload} />}
        </section>
        {row.quotas.length > 0 && <section aria-label={t("usage.detail.limits")}>
          <h4>{t("usage.detail.limits")}</h4>
          <div className="quota">{row.quotas.map(quota => {
            const reading = quotaReading(quota.bucket, quota.state);
            const { spent, left } = reading;
            const tone = quotaTone(reading, overview?.blockedBy ?? null, reserve);
            const reset = reading.confirmed && quota.bucket.resetsAt !== null && Number.isFinite(quota.bucket.resetsAt) && quota.bucket.resetsAt > 0 ? quota.bucket.resetsAt : null;
            const shown = !reading.confirmed
              ? (spent ? t("usage.quota.lastSeenSpent") : left === null ? "—" : t("usage.quota.lastSeen", { value: formatPercent(left) }))
              : spent ? t("usage.quota.spent") : left === null ? "—" : t("usage.quota.left", { value: formatPercent(left) });
            return <div className={`q ${tone} ${quota.bucket.model ? "sub" : ""} ${reading.confirmed ? "" : "stale"}`} key={quota.label} title={t("usage.quotaTitle", { label: limitLabel(quota.bucket.label), observed: fullTime(quota.bucket.observedAt), reset: fullTime(quota.bucket.resetsAt) })}>
              <span>{limitLabel(qualifiedLabel(quota.label, quota.bucket.model))}</span>
              <div className="bar"><i style={{ width: `${left ?? 0}%` }} /></div>
              <b>{shown}</b>
              <small className="quota-reset">
                {!reading.confirmed ? (reading.observedAt !== null ? <span title={fullTime(reading.observedAt)}>{t("usage.quota.unconfirmed", { time: relativeTime(reading.observedAt) })}</span> : null) : <>
                  {reset !== null ? <time dateTime={new Date(reset).toISOString()}>{t("usage.quota.reset", { time: fullTime(reset) })}</time> : <span>{t("usage.when.unknown")}</span>}
                  {reading.observedAt !== null && <span title={fullTime(reading.observedAt)}> · {t("usage.quota.seen", { time: relativeTime(reading.observedAt) })}</span>}
                </>}
              </small>
            </div>;
          })}</div>
          <ul className="acc-sources" aria-label={t("usage.detail.sources")}>{sources(row).map(source => <li key={source.name} title={fullTime(source.observedAt)}>{source.name} · {relativeTime(source.observedAt)}</li>)}</ul>
        </section>}
      </div>
      {row.detail.length > 0 && <section aria-label={t("usage.detail.blocks")}>
        <h4>{t("usage.detail.blocks")}</h4>
        <ul className="acc-blocks">{row.detail.map(line => <li key={line}>{line}</li>)}</ul>
      </section>}
      {props.bridgeOn && <section aria-label={t("usage.detail.activity", { period: props.periodLabel })}>
        <h4>{t("usage.detail.activity", { period: props.periodLabel })}</h4>
        {row.cells.length ? <div className="models">{row.cells.map(cell => <div className="cell" key={cell.key} style={{ "--c": providerColor[provider], "--heat": Math.min(1, cell.total / props.peak).toFixed(2) } as CSSProperties}>
          <div className="head">{cell.now > 0 && <span className="live" />}<span className="name">{cell.name}</span><span className="n">{cell.route !== "bridge" ? <small>{t("usage.countUnknown")}</small> : <>{cell.total}<small>{t("usage.countUnit")}</small></>}</span></div>
          {cell.route !== "bridge" ? <div className="cell-gap" /> : <Sparkline series={cell.series} peak={props.peak} provider={provider} width={220} height={26} />}
          <div className="where">{masked ? cell.projects.length > 0 && <span className="tag">{t("usage.projectsHidden", { count: cell.projects.length })}</span> : cell.projects.map(cwd => <span className="tag" key={cwd} title={cwd}>{projectName(cwd, masked)}</span>)}{cell.route !== "bridge" && <span className={`tag ${cell.route === "direct" ? "direct" : "neutral"}`}>{t(cell.route === "direct" ? "usage.direct.tag" : "usage.routeUnknown.tag")}</span>}{cell.roles.map(role => <span className="tag" key={role}>{role}</span>)}</div>
        </div>)}</div> : <div className="empty">{props.usageUnavailable ? t(props.usageError ? "usage.readFailed" : "common.checking") : t("usage.noCalls", { period: props.periodLabel })}</div>}
      </section>}
  </div>;
}

/// 한도 관측의 출처와 가장 최근 관측 시각(출처별). 서비스가 보낸 출처 문구를 그대로 보인다.
function sources(row: UsageRow): { name: string; observedAt: number }[] {
  const latest: Record<string, number> = {};
  for (const quota of row.quotas) latest[quota.bucket.source] = Math.max(latest[quota.bucket.source] ?? 0, quota.bucket.observedAt);
  return Object.entries(latest).map(([name, observedAt]) => ({ name, observedAt })).sort((a, b) => b.observedAt - a.observedAt || a.name.localeCompare(b.name));
}
