import { useCallback, useEffect, useRef, useState } from "react";
import { bucketState } from "./limits";
import { bridgeUsage, ompBridgeAction, rpc, toApiError } from "./api";
import type { BridgeUsage, OmpBridgeStatus } from "./api";
import { PROVIDER_ORDER } from "./types";
import type { Account, ApiError, QuotaBucket, Snapshot } from "./types";
import { accountLabel, intlLocale, lookup, t } from "./i18n";
import { whenBand } from "./overview";

/// 화면에서 쓰는 공급자 이름. 키는 계정 저장소의 provider 값(anthropic/openai/google/xai/other)이다.
export const providerNames: Record<string, string> = { anthropic: "Anthropic", openai: "OpenAI Codex", google: "Google Antigravity", xai: "xAI", other: "Z.AI" };
/// 공급자별 색 토큰(styles.css `--p-*`).
export const providerColor: Record<string, string> = { anthropic: "var(--p-claude)", openai: "var(--p-codex)", google: "var(--p-gemini)", xai: "var(--p-grok)", other: "var(--p-zai)" };
export const toolNames: Record<string, string> = { claude: "Claude Code", codex: "Codex", omp: "omp" };
/// 브릿지·omp가 쓰는 원래 공급자 ID(crates/service/src/bridge.rs ALIASES)와 ojak-* 별칭을 계정 저장소의 provider 값으로 맞춘다.
/// aam-*는 이름을 바꾸기 전 기록된 사용량을 같은 공급자로 보여 주기 위해 남긴다.
const providerAliases: Record<string, string> = {
  "ojak-claude": "anthropic", "aam-claude": "anthropic", "openai-codex": "openai", "ojak-codex": "openai", "aam-codex": "openai",
  "google-antigravity": "google", "ojak-antigravity": "google", "aam-antigravity": "google",
  "xai-oauth": "xai", "ojak-grok": "xai", "aam-grok": "xai", zai: "other", "ojak-zai": "other", "aam-zai": "other",
};
export function canonicalProvider(id: string): string {
  return providerAliases[id] ?? (PROVIDER_ORDER.includes(id) ? id : "other");
}
export const occupiedStates: Record<string, boolean> = { PREPARED: true, STARTING: true, ACTIVE: true, SUSPECT: true, ORPHANED: true };
/// 세션 상태 코드의 표시 이름. 사전에 없는 코드는 그대로 보여 준다.
export function sessionLabel(state: string): string {
  return lookup("session.state", state) ?? state;
}
/// 인증 근거 코드의 표시 이름. 사전에 없는 코드는 "실제 계정 미확인"으로 다룬다.
export function verificationLabel(verification: string): string {
  return lookup("verification", verification) ?? t("sessions.detail.verificationUnknown");
}

export function privacyText(text: string | null | undefined, masked: boolean): string {
  if (!text) return "—";
  return masked ? text.replace(/[\w.+-]+@[\w.-]+\.[\w-]+/g, t("privacy.emailHidden")) : text;
}

/// 캐릭터(깍이·호랑이) 그림 표시. 문구는 그대로 두고 그림만 끈다. 업무 화면에서 그림을 원치 않는 사용자를 위한 설정이다.
const CHARACTERS_KEY = "ojak.characters";
const characterListeners = new Set<(on: boolean) => void>();
export function charactersEnabled(): boolean {
  try { return localStorage.getItem(CHARACTERS_KEY) !== "off"; } catch { return true; }
}
export function setCharactersEnabled(on: boolean): void {
  try { if (on) localStorage.removeItem(CHARACTERS_KEY); else localStorage.setItem(CHARACTERS_KEY, "off"); } catch { /* 저장이 막혀도 이번 창에는 적용한다. */ }
  characterListeners.forEach(listener => listener(on));
}
export function useCharacters(): boolean {
  const [on, setOn] = useState(charactersEnabled);
  useEffect(() => { characterListeners.add(setOn); return () => { characterListeners.delete(setOn); }; }, []);
  return on;
}

/// 작업 폴더의 마지막 이름. 가림 모드에서는 경로 대신 고정 문구를 쓴다.
export function projectName(cwd: string | null | undefined, masked: boolean): string {
  if (!cwd) return t("project.unknown");
  if (masked) return t("privacy.projectHidden");
  return cwd.split("/").filter(Boolean).pop() || cwd;
}

const absoluteFormat = new Intl.DateTimeFormat(intlLocale, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", timeZoneName: "short" });
const unitFormat = (unit: "minute" | "hour" | "day") => new Intl.NumberFormat(intlLocale, { style: "unit", unit, unitDisplay: "short" });
const minutes = unitFormat("minute"), hours = unitFormat("hour"), days = unitFormat("day");

export function absoluteTime(value: number | null | undefined): string {
  if (!value || !Number.isFinite(value)) return t("time.noRecord");
  return absoluteFormat.format(value);
}

const weekdayFormat = new Intl.DateTimeFormat(intlLocale, { weekday: "short", hour: "2-digit", minute: "2-digit" });
const dateFormat = new Intl.DateTimeFormat(intlLocale, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
const fullFormat = new Intl.DateTimeFormat(intlLocale, { year: "numeric", month: "short", day: "numeric", weekday: "short", hour: "2-digit", minute: "2-digit" });

/// 줄에 쓰는 짧은 시각: 하루 안이면 남은 시간, 일주일 안이면 요일·시각, 그 뒤는 날짜·시각. 시간대는 붙이지 않는다(화면 아래에 한 번만 적는다).
export function shortTime(value: number, now = Date.now()): string {
  const band = whenBand(value, now);
  return band === "pending" ? t("time.resetPending") : band === "relative" ? relativeTime(value, true) : (band === "weekday" ? weekdayFormat : dateFormat).format(value);
}

/// 자세히 보기에 쓰는 전체 날짜·시각. 시간대는 붙이지 않는다.
export function fullTime(value: number | null | undefined): string {
  return !value || !Number.isFinite(value) ? t("time.noRecord") : fullFormat.format(value);
}

/// 화면의 시각이 따르는 시간대(예: `Asia/Seoul (GMT+9)`).
export function timeZoneLabel(): string {
  const zone = new Intl.DateTimeFormat(intlLocale).resolvedOptions().timeZone;
  const short = new Intl.DateTimeFormat(intlLocale, { timeZoneName: "short" }).formatToParts(Date.now()).find(part => part.type === "timeZoneName")?.value;
  return short && short !== zone ? `${zone} (${short})` : zone;
}

/// 지난 시간(또는 `future`면 남은 시간)을 "N분 전"/"in N min"처럼 현재 언어로 만든다. 한 시간 이상이면 분, 하루 이상이면 시간까지 붙인다.
export function relativeTime(value: number | null | undefined, future = false): string {
  if (!value) return t("time.unknown");
  const diff = future ? value - Date.now() : Date.now() - value;
  if (future && diff <= 0) return t("time.resetPending");
  if (diff < 60_000) return future ? t("time.withinMinute") : t("time.justNow");
  const elapsedMinutes = Math.floor(diff / 60_000);
  const elapsedHours = Math.floor(elapsedMinutes / 60);
  const duration = elapsedMinutes < 60 ? minutes.format(elapsedMinutes)
    : elapsedHours < 24 ? `${hours.format(elapsedHours)} ${minutes.format(elapsedMinutes % 60)}`
    : `${days.format(Math.floor(elapsedHours / 24))} ${hours.format(elapsedHours % 24)}`;
  return t(future ? "time.in" : "time.ago", { duration });
}


export { bucketState };

export function nextReset(buckets: QuotaBucket[], staleAfter: number): number | null {
  const now = Date.now();
  let next: number | null = null;
  for (const bucket of buckets) {
    const state = bucketState(bucket, staleAfter);
    if ((state === "known" || state === "exhausted") && bucket.observedAt > 0
      && now - bucket.observedAt <= staleAfter * 1000 && bucket.resetsAt !== null
      && bucket.resetsAt > now && (next === null || bucket.resetsAt < next)) next = bucket.resetsAt;
  }
  return next;
}

export interface AccountGroup {
  key: string;
  primary: Account;
  members: Account[];
  email: string | null;
  buckets: QuotaBucket[];
  /// 브릿지 gateway·요청 로그의 계정 문자열(email 또는 `account:<id>`)과 맞추는 소문자 키.
  keys: string[];
}

/// 도구마다 신원 문자열 형식이 다르므로 확인된 워크스페이스·OAuth pin으로 같은 계정을 찾는다.
/// 근거가 없는 연결은 합치지 않고 따로 표시한다.
function identityTokens(account: Account): string[] {
  const tokens: string[] = [];
  for (const pin of account.ompCredentialPins ?? []) tokens.push(`pin:${pin.provider}:${pin.hash}`);
  const segments = (account.identityKey || "").split("|").slice(1);
  const value = (name: string) => segments.find(segment => segment.startsWith(`${name}:`))?.slice(name.length + 1) || "";
  const workspace = value("workspace");
  if (workspace) tokens.push(`workspace:${account.provider}:${workspace}`);
  const subject = value("subject");
  if (subject) tokens.push(`subject:${account.provider}:${subject}`);
  // 워크스페이스를 확인하지 못한 연결만 이메일로 연결한다. 같은 이메일의 다른 워크스페이스를 합치지 않기 위함이다.
  if (!workspace && account.email) tokens.push(`email:${account.provider}:${account.email.toLowerCase()}`);
  return tokens;
}

export function groupAccounts(accounts: Account[]): AccountGroup[] {
  const owner = new Map<string, string>();
  const merged = new Map<string, string>();
  const root = (key: string): string => {
    const parent = merged.get(key);
    if (!parent || parent === key) return key;
    const resolved = root(parent);
    merged.set(key, resolved);
    return resolved;
  };
  for (const account of accounts) {
    merged.set(account.id, account.id);
    for (const token of identityTokens(account)) {
      const existing = owner.get(token);
      if (existing) merged.set(root(account.id), root(existing));
      else owner.set(token, account.id);
    }
  }
  // 신원 근거가 없는 지난 관측 행은 따로 계정을 만들지 않고, 같은 공급자·이메일의
  // 확인된 계정이 정확히 하나일 때만 그 계정 밑에 붙인다. 확인된 계정끼리는 이메일로 합치지 않는다.
  const verifiedByEmail = new Map<string, Set<string>>();
  for (const account of accounts) {
    if (!account.identityKey || !account.email) continue;
    const key = `${account.provider}:${account.email.toLowerCase()}`;
    verifiedByEmail.set(key, (verifiedByEmail.get(key) ?? new Set()).add(root(account.id)));
  }
  for (const account of accounts) {
    if (account.identityKey || account.ompCredentialPins?.length || !account.email) continue;
    const owners = verifiedByEmail.get(`${account.provider}:${account.email.toLowerCase()}`);
    if (owners?.size === 1) merged.set(root(account.id), [...owners][0]);
  }
  const grouped = new Map<string, Account[]>();
  for (const account of accounts) {
    const key = root(account.id);
    grouped.set(key, [...(grouped.get(key) || []), account]);
  }
  return [...grouped].map(([key, members]) => {
    const ordered = [...members].sort((a, b) => Number(b.canLaunch) - Number(a.canLaunch)
      || Number(a.tool === "omp") - Number(b.tool === "omp")
      || a.label.localeCompare(b.label));
    // 같은 계정의 연결은 같은 구독을 공유하므로 버킷을 한 번만 쓴다.
    const buckets: QuotaBucket[] = [];
    const keys = new Set<string>();
    for (const member of ordered) {
      for (const bucket of member.buckets) {
        const index = buckets.findIndex(item => item.id === bucket.id);
        if (index < 0) buckets.push(bucket);
        else if (bucket.observedAt > buckets[index].observedAt) buckets[index] = bucket;
      }
      if (member.email) keys.add(member.email.toLowerCase());
      // xAI처럼 이메일 대신 계정 ID로 gateway를 여는 공급자는 identityKey의 subject로 맞춘다.
      const subject = (member.identityKey || "").split("|").find(segment => segment.startsWith("subject:"))?.slice(8);
      if (subject) keys.add(`account:${subject.toLowerCase()}`);
    }
    return { key, primary: ordered[0], members: ordered, email: ordered.find(member => member.email)?.email ?? null, buckets, keys: [...keys] };
  });
}

/// 계정 저장소에서 "지금 존재하는" 계정만 남긴다. 다시 관측되지 않은 omp 기록만으로 이루어진 묶음은 제외한다.
export function isCurrentGroup(group: AccountGroup): boolean {
  return group.members.some(member => member.canLaunch || member.identityKey || member.buckets.length > 0);
}

export function useAction() {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const locked = useRef(false);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  async function run<T>(task: () => Promise<T>, success?: (result: T) => string | void): Promise<boolean> {
    if (locked.current) return false;
    locked.current = true;
    setPending(true); setError(null); setMessage(null);
    try {
      const result = await task();
      if (mounted.current) setMessage(success?.(result) || null);
      return true;
    } catch (failure) {
      if (mounted.current) setError(toApiError(failure));
      return false;
    } finally {
      locked.current = false;
      if (mounted.current) setPending(false);
    }
  }
  return { pending, error, message, run, clear: () => { setError(null); setMessage(null); } };
}

export function useSnapshot() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [connecting, setConnecting] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const alive = useRef(false);
  const flight = useRef<Promise<void> | null>(null);
  const refreshLock = useRef(false);

  const load = useCallback(async (refresh = false): Promise<void> => {
    if (refresh && refreshLock.current) return;
    if (!refresh && flight.current) return flight.current;
    if (refresh) { refreshLock.current = true; setRefreshing(true); }
    try {
      if (flight.current) await flight.current;
      if (!alive.current) return;
      const request = (async () => {
        try {
          // 새로고침 버튼만 공급자 캐시를 건너뛴다. 3초 주기 읽기는 서비스의 마지막 관측만 읽는다.
          const result = await rpc<Snapshot>(refresh ? "quota.refresh" : "status.read", refresh ? { force: true } : {});
          if (result.version !== 1) throw { code: "PROTOCOL_MISMATCH", message: t("api.protocolMismatch"), retryable: false };
          // 라벨은 화면 표시용이라 서비스에 되돌려 보내지 않는다. 자동 발견 계정의 `기본 프로필`을 여기서 한 번만 옮긴다.
          const accounts = result.accounts.map(account => ({ ...account, label: accountLabel(account.label) }));
          if (alive.current) { setSnapshot({ ...result, accounts }); setError(null); }
        } catch (failure) {
          if (alive.current) setError(toApiError(failure));
        } finally {
          if (alive.current) setConnecting(false);
        }
      })();
      flight.current = request;
      await request;
      if (flight.current === request) flight.current = null;
    } finally {
      if (refresh) { refreshLock.current = false; if (alive.current) setRefreshing(false); }
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    void load();
    const interval = window.setInterval(() => { if (!document.hidden && !refreshLock.current) void load(); }, 3000);
    const onVisible = () => { if (!document.hidden) void load(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { alive.current = false; window.clearInterval(interval); document.removeEventListener("visibilitychange", onVisible); };
  }, [load]);
  return { snapshot, error, connecting, refreshing, reload: load };
}

/// 화면을 오갈 때 빈 화면이 잠깐 보이지 않도록 마지막 브릿지 응답을 기억한다.
let bridgeCache: { status: OmpBridgeStatus | null; usage: BridgeUsage | null; usageWindow: number | null } = { status: null, usage: null, usageWindow: null };

/// 브릿지 상태와(요청 시) 사용 현황을 5초마다 다시 읽는다. `windowMinutes`가 null이면 상태만 읽는다.
export function useBridge(windowMinutes: number | null) {
  const [status, setStatus] = useState<OmpBridgeStatus | null>(bridgeCache.status);
  const [usage, setUsage] = useState<BridgeUsage | null>(bridgeCache.usageWindow === windowMinutes ? bridgeCache.usage : null);
  const [usageWindow, setUsageWindow] = useState(bridgeCache.usageWindow);
  const [error, setError] = useState<ApiError | null>(null);
  const [loaded, setLoaded] = useState(bridgeCache.status !== null);
  useEffect(() => {
    let active = true;
    let busy = false;
    setLoaded(false);
    setError(null);
    const load = async () => {
      if (busy || document.hidden) return;
      busy = true;
      try {
        const [nextStatus, nextUsage] = await Promise.all([ompBridgeAction("status"), windowMinutes === null ? Promise.resolve(null) : bridgeUsage(windowMinutes)]);
        if (!active) return;
        bridgeCache = { status: nextStatus, usage: nextUsage ?? bridgeCache.usage, usageWindow: windowMinutes ?? bridgeCache.usageWindow };
        setStatus(nextStatus);
        if (nextUsage) { setUsage(nextUsage); setUsageWindow(windowMinutes); }
        setError(null);
      } catch (failure) {
        if (active) setError(toApiError(failure));
      } finally {
        busy = false;
        if (active) setLoaded(true);
      }
    };
    void load();
    const interval = window.setInterval(() => { void load(); }, 5000);
    const onVisible = () => { if (!document.hidden) void load(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { active = false; window.clearInterval(interval); document.removeEventListener("visibilitychange", onVisible); };
  }, [windowMinutes]);
  return { status, usage: usageWindow === windowMinutes ? usage : null, error, loaded: loaded && (windowMinutes === null || usageWindow === windowMinutes || Boolean(error)) };
}
