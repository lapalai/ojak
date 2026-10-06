import { useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { listen, emit } from "@tauri-apps/api/event";
import { ArrowUpRight, RefreshCw } from "lucide-react";
import { native, updatesStatus } from "./api";
import type { UpdateStatus } from "./api";
import { bucketState, canonicalProvider, groupAccounts, isCurrentGroup, providerColor, providerNames, relativeTime, useBridge, useSnapshot } from "./state";
import { limitsOf, remainingTone as tone, tightestOf } from "./limits";
import type { Limit } from "./limits";
import type { Snapshot } from "./types";
import ojakIcon from "./assets/ojak-icon.png";
import { t, limitLabel } from "./i18n";
import { UPDATE_CHECK_INTERVAL_MS, updateBadgeVisible } from "./updates";

/// 메뉴바 팝오버: "지금 쓰는 서비스가 얼마나 남았나"에만 답한다. 계정 수·요청 수는 대시보드 몫이다.
/// 공급자(Claude·Codex·Gemini)마다 카드 하나. 계정은 그 카드 안의 줄이다. 계정마다 카드를 나누면
/// 서로 다른 서비스처럼 읽히기 때문이다. 사용자가 물은 세 공급자는 항상, 그 밖의 공급자는 쓸 때만 보인다.
const FOCUS_PROVIDERS = ["anthropic", "openai", "google"];
const IN_USE_MS = 15 * 60_000;
const WIDTH = 340;
const MAX_HEIGHT = 560;
/// 좁은 팝오버용 짧은 공급자 이름. 사용자가 부르는 이름을 쓴다.
const SHORT_NAMES: Record<string, string> = { anthropic: "Claude", openai: "Codex", google: "Gemini", xai: "Grok", other: "Z.AI" };

interface AccountLine { key: string; name: string; models: string[]; limits: Limit[]; tightest: number | null; inUse: boolean }
interface ProviderCard { provider: string; accounts: AccountLine[]; tightest: number | null; inUse: boolean }

function buildCards(snapshot: Snapshot, sessions: { provider: string; email: string; model: string; lastUsedAt: number }[], masked: boolean): ProviderCard[] {
  const now = Date.now();
  const staleAfter = snapshot.policy.staleAfterSeconds;
  const cards = new Map<string, ProviderCard>();
  for (const group of groupAccounts(snapshot.accounts).filter(isCurrentGroup)) {
    const provider = canonicalProvider(group.primary.provider);
    const ids = new Set(group.members.map(member => member.id));
    const models = new Set<string>();
    for (const session of sessions) {
      if (now - session.lastUsedAt <= IN_USE_MS && canonicalProvider(session.provider) === provider && group.keys.includes(session.email.toLowerCase())) models.add(session.model);
    }
    for (const session of snapshot.sessions) if (session.state === "ACTIVE" && ids.has(session.accountId) && session.model !== "native-default") models.add(session.model);
    const limits = limitsOf(group.buckets, [...models], bucket => bucketState(bucket, staleAfter));
    const card = cards.get(provider) ?? { provider, accounts: [], tightest: null, inUse: false };
    const line = { key: group.key, name: group.email ?? "", models: [...models], limits, tightest: limits.length ? tightestOf(limits) : null, inUse: models.size > 0 };
    card.accounts.push(line);
    card.inUse ||= line.inUse;
    cards.set(provider, card);
  }
  return [...cards.values()]
    .filter(card => FOCUS_PROVIDERS.includes(card.provider) || card.inUse)
    .map(card => {
      // 이름은 공급자 안에서 번호를 매긴다. 줄 순서: 쓰는 계정 먼저, 그 안에서 적게 남은 순.
      card.accounts.sort((a, b) => a.name.localeCompare(b.name)).forEach((line, index) => { if (masked || !line.name) line.name = t("popover.account", { index: index + 1 }); });
      card.accounts.sort((a, b) => Number(b.inUse) - Number(a.inUse) || (a.tightest ?? 101) - (b.tightest ?? 101));
      // 카드 대표값: 쓰고 있는 계정이 있으면 그중 가장 적게 남은 값, 없으면 전체에서.
      const pool = card.inUse ? card.accounts.filter(line => line.inUse) : card.accounts;
      const values = pool.map(line => line.tightest).filter((value): value is number => value !== null);
      card.tightest = values.length ? Math.min(...values) : null;
      return card;
    })
    .sort((a, b) => Number(b.inUse) - Number(a.inUse) || (FOCUS_PROVIDERS.indexOf(a.provider) + 1 || 99) - (FOCUS_PROVIDERS.indexOf(b.provider) + 1 || 99));
}

function Bar({ value, reserve, small = false }: { value: number | null; reserve: number; small?: boolean }) {
  return <span className={`pop-meter${small ? " small" : ""} ${tone(value, reserve)}`} role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={value ?? undefined}>
    <i style={{ width: `${value ?? 0}%` }} />
    {!small && <b style={{ left: `${reserve}%` }} aria-hidden="true" />}
  </span>;
}

function Line({ line, reserve }: { line: AccountLine; reserve: number }) {
  const [open, setOpen] = useState(false);
  const reset = line.limits.map(limit => limit.resetsAt).filter((value): value is number => value !== null && value > 0).sort((a, b) => a - b)[0];
  return <div className={`pop-account${line.inUse ? " in-use" : ""}`}>
    <button type="button" className="pop-account-head" onClick={() => setOpen(!open)} aria-expanded={open}>
      <span className="pop-account-name" title={line.models.join(", ") || undefined}>{line.name}</span>
      <Bar value={line.tightest} reserve={reserve} small />
      <span className={`pop-limit-value ${tone(line.tightest, reserve)}`}>{line.tightest === null ? "—" : `${Math.round(line.tightest)}%`}</span>
      {reset !== undefined && <span className="pop-limit-reset">{reset <= Date.now() ? t("time.resetPending") : relativeTime(reset, true)}</span>}
    </button>
    {open && line.models.length > 0 && <div className="pop-account-model">{line.models.join(", ")}</div>}
    {open && line.limits.map(limit => <div key={limit.key} className={`pop-limit${limit.relevant ? "" : " dim"}`}>
      <span className="pop-limit-label">{limitLabel(limit.label)}</span>
      <Bar value={limit.remaining} reserve={reserve} />
      <span className={`pop-limit-value ${tone(limit.remaining, reserve)}`}>{limit.remaining === null ? t("popover.check") : `${Math.round(limit.remaining)}%`}</span>
      <span className="pop-limit-reset">{limit.resetsAt && limit.remaining !== null ? relativeTime(limit.resetsAt, true) : ""}</span>
    </div>)}
  </div>;
}

export function Popover() {
  const { snapshot, error, reload, refreshing } = useSnapshot();
  const { status } = useBridge(null);
  const [masked, setMasked] = useState(() => { try { return localStorage.getItem("aam.privacy") !== "visible"; } catch { return true; } });
  const root = useRef<HTMLDivElement>(null);
  const latest = useRef(snapshot);
  latest.current = snapshot;

  useEffect(() => {
    const readPrivacy = () => { try { setMasked(localStorage.getItem("aam.privacy") !== "visible"); } catch { setMasked(true); } };
    const shown = listen("ojak://popover-shown", readPrivacy);
    // 열 때 사용량이 오래됐으면(잠자기에서 깨어난 직후 등) 바로 다시 조회한다. "확인 필요"가 남지 않게.
    const refreshIfStale = listen("ojak://popover-shown", () => {
      const current = latest.current;
      if (current && Date.now() - (current.lastRefreshAt ?? 0) > current.policy.staleAfterSeconds * 1000) void reload(true);
    });
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape") void native("hide_popover"); };
    window.addEventListener("keydown", onKey);
    return () => { window.removeEventListener("keydown", onKey); void shown.then(off => off()); void refreshIfStale.then(off => off()); };
  }, [reload]);

  // 내용 높이에 맞춰 창 크기를 맞춘다. 메뉴바 팝오버는 빈 공간 없이 딱 맞아야 가볍게 보인다.
  useEffect(() => {
    const element = root.current;
    if (!element) return;
    const observer = new ResizeObserver(() => { void getCurrentWindow().setSize(new LogicalSize(WIDTH, Math.min(MAX_HEIGHT, Math.ceil(element.scrollHeight)))); });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const cards = useMemo(() => snapshot ? buildCards(snapshot, status?.bridge?.sessions ?? [], masked) : [], [snapshot, status, masked]);
  const reserve = snapshot?.policy.safetyReservePercent ?? 10;

  const [updateStatus, setUpdateStatus] = useState<UpdateStatus | null>(null);
  useEffect(() => {
    const apply = (status: UpdateStatus) => setUpdateStatus(status);
    void updatesStatus(false).then(apply).catch(() => setUpdateStatus(null));
    const timer = window.setInterval(() => { void updatesStatus(false).then(apply).catch(() => undefined); }, UPDATE_CHECK_INTERVAL_MS);
    const unlisten = listen<UpdateStatus>("ojak://update-status", event => apply(event.payload));
    return () => { window.clearInterval(timer); void unlisten.then(off => off()); };
  }, []);

  return <div className="popover" ref={root}>
    <header className="pop-header">
      <img src={ojakIcon} alt="" width={22} height={22} />
      <strong>{t("popover.title")}</strong>
      {updateBadgeVisible(updateStatus) && <button type="button" className="pop-update" onClick={() => { void native("open_dashboard").then(() => emit("ojak://settings")); }}>{t("popover.update")}</button>}
      <span className="pop-meta">{refreshing || snapshot?.refreshing ? t("usage.refreshing") : snapshot?.lastRefreshAt ? relativeTime(snapshot.lastRefreshAt) : ""}</span>
      <button type="button" className="pop-refresh" onClick={() => { void reload(true); }} disabled={!snapshot || refreshing || snapshot.refreshing} title={t("refresh.title")} aria-label={t("refresh.aria")}><RefreshCw size={13} className={refreshing || snapshot?.refreshing ? "spinner" : undefined} /></button>
    </header>
    {!snapshot ? <p className="pop-empty">{error ? t("popover.offline") : t("common.checking")}</p> : <div className="pop-body">
      {cards.length === 0 && <p className="pop-empty">{t("popover.none")}</p>}
      {cards.map(card => <article key={card.provider} className="pop-card">
        <div className="pop-card-head">
          <span className="pop-dot" style={{ background: providerColor[card.provider] }} />
          <span className="pop-provider" title={providerNames[card.provider]}>{SHORT_NAMES[card.provider] ?? providerNames[card.provider]}</span>
          {card.inUse && <span className="pop-live">{t("popover.inUse")}</span>}
          <span className={`pop-big ${tone(card.tightest, reserve)}`}>{card.tightest === null ? "—" : `${Math.round(card.tightest)}%`}<small>{t("popover.left")}</small></span>
        </div>
        {card.accounts.map(line => <Line key={line.key} line={line} reserve={reserve} />)}
      </article>)}
    </div>}
    <footer className="pop-footer">
      <span>{t("popover.hint")}</span>
      <button type="button" onClick={() => { void native("open_dashboard"); }}>{t("popover.openDashboard")}<ArrowUpRight size={13} /></button>
    </footer>
  </div>;
}
