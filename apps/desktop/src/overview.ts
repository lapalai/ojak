import { bucketState, quotaReading, remainingTone, WARN_REMAINING_PERCENT } from "./limits.ts";
import type { QuotaReading } from "./limits.ts";
import { dictionaries } from "./i18n.ts";
import type { AccountGroup } from "./state.ts";
import type { Account, AccountQuotaSummary as Verdict, QuotaBucket } from "./types.ts";

/// 계정 한 줄이 접힌 상태에서 말해야 할 것(상태·가장 중요한 한도·다음 시각)을 정하는 순수 규칙.
/// 문구는 화면(AccountRow)이 번역하고, 여기서는 무엇을 말할지만 결정한다.

export interface ModelCell { key: string; name: string; series: number[]; total: number; now: number; projects: string[]; route: "bridge" | "direct" | "unknown"; roles: string[] }
export interface Quota { label: string; value: number | null; state: string; bucket: QuotaBucket }
export interface UsageRow {
  key: string; provider: string; account: string | null; group: AccountGroup | null; cells: ModelCell[]; quotas: Quota[]; verdict: Verdict | null;
  /// 서비스가 알려 준 차단 원문(범위·사유·시각). 원인 해석 없이 자세히 보기에만 둔다.
  detail: string[]; notes: string[]; off: boolean;
  /// 계정을 알 수 없는 경로 기록(Ojak을 거치지 않은 호출 등)이다.
  unknown: boolean;
  /// 사용자가 붙인 의미 있는 이름. 없으면 이메일이 이름이다.
  alias: string | null;
  /// 정렬용 키. 사용량이 아니라 이름이라 값이 바뀌어도 줄이 뛰지 않는다.
  sortKey: string;
}

export type RowState = Verdict["kind"] | "availableLow";
export type Tone = "good" | "warning" | "danger" | "neutral";
export type WindowKind = "weekly" | "fiveHour" | "other";
export type Blocker = { kind: "weekly" | "fiveHour" | "rate" | "generic" } | { kind: "limit"; label: string };
export type When = { kind: "recovery" | "reset" | "modelBack" | "unknown"; at: number | null } | { kind: "expiring"; at: number; percent: number };

export interface CriticalLimit { label: string; left: number | null; spent: boolean; resetsAt: number | null; model: string | null }
export interface Overview {
  state: RowState;
  tone: Tone;
  /// `quota`: 한도 소진으로 새 작업을 받지 않음, `account`: 로그인·제외 때문에 한도와 상관없이 받지 않음.
  blockedBy: "quota" | "account" | null;
  /// 접힌 줄에 보일 공용 한도(모델 구분 없는 한도). 신선하게 확인한 값만 쓴다. 막힌 계정에서는 막은 한도 하나다.
  limit: CriticalLimit | null;
  /// 모델 전용 한도 중 가장 적게 남은 것. 공용 한도가 없거나 그보다 적게 남은 경우에만 있고, 라벨에 모델 이름이 들어 있다.
  modelLimit: CriticalLimit | null;
  blocker: Blocker | null;
  /// 일부 모델 제한일 때 이름을 말해야 하는 모델.
  models: string[];
  when: When | null;
}

/// 모델 전용 한도는 모델 이름을 앞에 붙여 계정 전체 한도로 읽히지 않게 한다. 라벨에 이미 모델 이름이 있으면 그대로 쓴다.
export function qualifiedLabel(label: string, model: string | null): string {
  return model && !label.toLowerCase().includes(model.toLowerCase()) ? `${model} ${label}` : label;
}

export function windowKind(label: string): WindowKind {
  const text = label.toLowerCase();
  if (/주간|weekly|7[\s-]?day|\b7d\b|\b1w\b/.test(text)) return "weekly";
  if (/5시간|5[\s-]?hour|\b5h\b/.test(text)) return "fiveHour";
  return "other";
}

/// 공급자 버킷 라벨(`Claude 5 Hour · 5시간`, `Gemini · 주간`, `Fable 주간`)을 짧은 표시 이름으로 줄인다.
/// 같은 계정 안에서 짧은 이름이 겹치는데 값까지 같으면 같은 한도를 두 도구가 본 것이므로 하나로 합치고,
/// 값이 다르면(Antigravity의 Gemini·Claude & GPT처럼) 앞부분을 붙여 구분한다.
export function quotaRows(buckets: QuotaBucket[], staleAfter: number, now = Date.now()): Quota[] {
  const parts = buckets.map(bucket => bucket.label.split(" · "));
  const shorts = parts.map(segments => segments[segments.length - 1]);
  const rows = new Map<string, Quota>();
  buckets.forEach((bucket, index) => {
    const siblings = buckets.filter((_, otherIndex) => otherIndex !== index && shorts[otherIndex] === shorts[index]);
    const sameValue = siblings.every(other => other.usedPercent !== null && bucket.usedPercent !== null && Math.abs(other.usedPercent - bucket.usedPercent) < 0.5);
    const duplicate = siblings.length > 0 && !sameValue && parts[index].length > 1;
    const label = duplicate ? `${parts[index][0]} ${shorts[index]}` : shorts[index];
    const state = bucketState(bucket, staleAfter, now);
    const value = bucket.usedPercent !== null && Number.isFinite(bucket.usedPercent) ? bucket.usedPercent : null;
    const existing = rows.get(label);
    if (!existing || (value ?? -1) > (existing.value ?? -1)) rows.set(label, { label, value, state, bucket });
  });
  return [...rows.values()];
}

// --- 계정 이름 -------------------------------------------------------------

const TOOL_LABELS = ["claude code", "codex", "oh my pi", "omp"];
const OMP_LABELS = ["omp", "oh my pi"];
const EMAIL = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
/// 서비스가 자동 발견 계정에 붙이는 기본 프로필 표기. 표시 언어마다 달라 모든 사전의 값을 받는다.
const DEFAULT_PROFILE = new Set(["기본 프로필", ...Object.values(dictionaries).map(dictionary => String(dictionary["account.defaultProfile"]))].map(text => text.toLowerCase()));

/// 사용자가 붙였을 법한 라벨만 돌려준다. `Claude Code · 기본 프로필 · a@b.c`, `OMP · anthropic`처럼
/// 도구와 공급자·이메일로만 이루어진 자동 라벨은 이름이 아니므로 null이다.
export function meaningfulLabel(label: string): string | null {
  const parts = label.split(" · ").map(part => part.trim()).filter(part => part && !EMAIL.test(part));
  if (!parts.length) return null;
  const [head, ...rest] = parts;
  const headTool = head.toLowerCase();
  if (TOOL_LABELS.includes(headTool) && rest.every((part, index) => DEFAULT_PROFILE.has(part.toLowerCase()) || (index === 0 && OMP_LABELS.includes(headTool) && /^[a-z0-9-]+$/i.test(part)))) return null;
  return parts.join(" · ");
}

/// 같은 계정 묶음의 이름: 구성원 중 의미 있는 라벨이 있는 첫 번째. 없으면 null이라 호출하는 쪽이 이메일을 쓴다.
export function groupAlias(members: readonly Account[]): string | null {
  for (const member of members) {
    const label = meaningfulLabel(member.label);
    if (label) return label;
  }
  return null;
}

/// 줄 순서. 사용량이 아니라 이름으로 정해 값이 바뀌어도 줄이 뛰지 않는다. 계정을 알 수 없는 경로 기록은 맨 뒤다.
export function compareRows(a: Pick<UsageRow, "key" | "unknown" | "group" | "sortKey">, b: Pick<UsageRow, "key" | "unknown" | "group" | "sortKey">): number {
  return Number(a.unknown) - Number(b.unknown) || Number(!a.group) - Number(!b.group) || a.sortKey.localeCompare(b.sortKey) || a.key.localeCompare(b.key);
}

// --- 펼침 ------------------------------------------------------------------

/// 펼침은 계정 ID로 기억한다. 묶음 키는 구성원이 바뀌면 달라질 수 있지만 구성원 ID는 그대로라 갱신 뒤에도 유지된다.
export function isExpanded(open: ReadonlySet<string>, ids: readonly string[]): boolean {
  return ids.some(id => open.has(id));
}
export function toggleExpanded(open: ReadonlySet<string>, ids: readonly string[]): Set<string> {
  const next = new Set(open);
  if (isExpanded(open, ids)) for (const id of ids) next.delete(id);
  else for (const id of ids) next.add(id);
  return next;
}

// --- 한도 고르기 -----------------------------------------------------------

const DAY_MS = 86_400_000;

function toLimit(quota: Quota, reading: QuotaReading): CriticalLimit {
  const reset = quota.bucket.resetsAt;
  return {
    label: quota.label, left: reading.left, spent: reading.spent, model: quota.bucket.model,
    resetsAt: reading.confirmed && reset !== null && Number.isFinite(reset) && reset > 0 ? reset : null,
  };
}

function confirmedQuotas(quotas: readonly Quota[]): { quota: Quota; reading: QuotaReading }[] {
  return quotas.map(quota => ({ quota, reading: quotaReading(quota.bucket, quota.state) })).filter(item => item.reading.confirmed && item.reading.left !== null);
}

function tightest(items: { quota: Quota; reading: QuotaReading }[]): CriticalLimit | null {
  const best = [...items].sort((a, b) => (a.reading.left as number) - (b.reading.left as number)
    || Number(windowKind(b.quota.label) === "weekly") - Number(windowKind(a.quota.label) === "weekly"))[0];
  return best ? toLimit(best.quota, best.reading) : null;
}

/// 계정 전체에 걸리는 한도 중 신선하게 확인한 가장 적게 남은 것. 모델 전용 한도는 섞지 않는다.
export function criticalLimit(quotas: readonly Quota[]): CriticalLimit | null {
  return tightest(confirmedQuotas(quotas).filter(item => !item.quota.bucket.model));
}

/// 모델 전용 한도 중 신선하게 확인한 가장 적게 남은 것. 계정 전체 한도로 읽히지 않도록 따로 둔다.
export function modelLimitOf(quotas: readonly Quota[]): CriticalLimit | null {
  return tightest(confirmedQuotas(quotas).filter(item => item.quota.bucket.model));
}

/// 새 작업을 막는 한도: 소진된 공용 한도 중 가장 늦게 풀리는 것(그 한도가 풀려야 다시 쓸 수 있다). 같으면 주간을 먼저 본다.
export function blockerLimit(quotas: readonly Quota[]): CriticalLimit | null {
  const spent = confirmedQuotas(quotas).filter(item => item.reading.spent && !item.quota.bucket.model);
  const reset = (item: { quota: Quota }) => item.quota.bucket.resetsAt ?? Number.NEGATIVE_INFINITY;
  const best = [...spent].sort((a, b) => reset(b) - reset(a) || Number(windowKind(b.quota.label) === "weekly") - Number(windowKind(a.quota.label) === "weekly"))[0];
  return best ? toLimit(best.quota, best.reading) : null;
}

function blockerOf(verdict: Verdict, exhausted: CriticalLimit | null): Blocker {
  if (verdict.rate) return { kind: "rate" };
  const label = exhausted?.label ?? (verdict.label ? verdict.label.split(" · ").pop() ?? verdict.label : null);
  if (!label) return { kind: "generic" };
  const kind = windowKind(label);
  return kind === "other" ? { kind: "limit", label } : { kind };
}

/// 일부 모델 제한이 풀리는 가장 이른 시각. 이미 지난 시각은 시계가 지난 것일 뿐 회복 확인이 아니므로 쓰지 않는다.
function modelRecovery(quotas: readonly Quota[], now: number): number | null {
  let earliest: number | null = null;
  for (const { quota, reading } of confirmedQuotas(quotas)) {
    const reset = quota.bucket.resetsAt;
    if (!quota.bucket.model || !reading.spent || reset === null || !Number.isFinite(reset) || reset <= now) continue;
    if (earliest === null || reset < earliest) earliest = reset;
  }
  return earliest;
}

const TONES: Record<RowState, Tone> = { available: "good", availableLow: "warning", reserve: "warning", partial: "warning", resting: "danger", excluded: "neutral", login: "danger", unknown: "neutral", credits: "warning", extra: "warning" };

export function overviewOf(verdict: Verdict, quotas: readonly Quota[], now: number): Overview {
  const kind = verdict.kind;
  const common = criticalLimit(quotas);
  const model = modelLimitOf(quotas);
  const low = kind === "available" && common !== null && !common.spent && common.left !== null && common.left <= WARN_REMAINING_PERCENT;
  const state: RowState = low ? "availableLow" : kind;
  const base = { state, tone: TONES[state], models: kind === "partial" ? verdict.models : [] };
  if (kind === "resting" || kind === "credits" || kind === "extra") {
    const exhausted = blockerLimit(quotas);
    // 쉬는 중은 다시 확인할 시각, 크레딧·추가 사용량은 구독 한도가 풀리는 시각이다.
    const at = verdict.until;
    const when: When = at === null ? { kind: "unknown", at: null } : { kind: kind === "resting" ? "recovery" : "reset", at };
    return { ...base, blockedBy: "quota", limit: exhausted, modelLimit: null, blocker: blockerOf(verdict, exhausted), when };
  }
  if (kind === "login" || kind === "excluded") return { ...base, blockedBy: "account", limit: null, modelLimit: null, blocker: null, when: null };
  if (kind === "unknown") return { ...base, blockedBy: null, limit: null, modelLimit: null, blocker: null, when: null };
  // 모델 전용 한도는 공용 한도가 없거나 그보다 눈에 띄게 적을 때만 따로 말한다.
  const modelNote = model && (common === null || (model.left !== null && model.left <= WARN_REMAINING_PERCENT && model.left < (common.left ?? 100))) ? model : null;
  if (kind === "partial") {
    const back = modelRecovery(quotas, now);
    return { ...base, blockedBy: null, limit: common, modelLimit: modelNote, blocker: null, when: { kind: back === null ? "unknown" : "modelBack", at: back } };
  }
  const expiring = verdict.expiring && verdict.expiring.resetsAt > now ? verdict.expiring : null;
  const when: When | null = expiring ? { kind: "expiring", at: expiring.resetsAt, percent: expiring.usablePercent }
    : common ? (common.resetsAt && common.resetsAt > now ? { kind: "reset", at: common.resetsAt } : { kind: "unknown", at: null }) : null;
  return { ...base, blockedBy: null, limit: common, modelLimit: modelNote, blocker: null, when };
}

/// 자세히 보기의 한도 막대 색. 새 작업을 받지 않는 계정의 남은 한도는 초록·주황이 되지 않게 흐리게 둔다.
/// 한도 소진으로 쉬는 계정에서는 막은 한도(소진)만 빨강으로 남는다.
export function quotaTone(reading: QuotaReading, blockedBy: Overview["blockedBy"], reserve: number): "ok" | "warn" | "bad" | "unknown" | "muted" {
  if (!reading.confirmed) return "unknown";
  if (blockedBy === "account") return "muted";
  if (reading.spent) return "bad";
  return blockedBy === "quota" ? "muted" : remainingTone(reading.left, reserve);
}

// --- 시각 ------------------------------------------------------------------

export type WhenBand = "pending" | "relative" | "weekday" | "date";
/// 짧은 시각 표기 방식: 지났으면 확인 중, 하루 안이면 남은 시간, 일주일 안이면 요일·시각, 그 뒤는 날짜·시각.
export function whenBand(at: number, now: number): WhenBand {
  const diff = at - now;
  if (diff <= 0) return "pending";
  if (diff < DAY_MS) return "relative";
  return diff < 6 * DAY_MS ? "weekday" : "date";
}

// --- 복구 알림 -------------------------------------------------------------

/// 복구 알림을 켤 수 있는 상태. 쉬는 중이거나 일부 모델만 막힌 묶음, 구독 한도를 다 써서 크레딧·추가 사용량으로 가는 묶음이다. 이미 켜 둔 것은 상태가 바뀌어도 끌 수 있게 보인다.
export function watchable(state: RowState): boolean {
  return state === "resting" || state === "partial" || state === "credits" || state === "extra";
}
/// 묶음 구성원 중 서비스가 감시 중으로 알려 준 ID.
export function watchedMembers(watched: readonly string[] | null, ids: readonly string[]): string[] {
  return watched ? ids.filter(id => watched.includes(id)) : [];
}

// --- 로그인 -----------------------------------------------------------------

/// 다시 로그인할 수 있는 구성원: 공식 CLI(Claude Code·Codex)가 로그인을 요구하는 계정. omp는 여기서 로그인하지 못한다.
export function loginTarget(members: readonly Account[], installed: (tool: string) => boolean): Account | null {
  return members.find(member => !member.canLaunch && member.authStatus === "auth-required" && (member.tool === "claude" || member.tool === "codex") && installed(member.tool)) ?? null;
}
