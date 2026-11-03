import type { Account, ApiCreditBillingPolicy, ApiCreditBillingStatus, ApiCreditGrantStatus, ApiCreditGrantPolicy, BillingCycle, ExpirySource, ProactiveBlock } from "./types.ts";

/// 서비스가 받는 상한(`policy.update`의 apiCreditGrant). 입력 오타를 거르는 값이고 의미는 서비스 검증이 정한다.
export const MAX_GRANT_BUDGET_USD = 1_000_000;
export const MAX_GRANT_DAYS = 366;
const DAY_MS = 86_400_000;

/// 서비스가 계산한 상태 중 화면이 쓰는 부분. 이전 서비스는 상태를 주지 않으므로 그때는 '알 수 없음'으로 본다.
export type GrantView = Pick<ApiCreditGrantStatus, "state" | "budgetUsd" | "usedUsd" | "remainingUsd" | "startsAt" | "expiresAt" | "unmeteredRequests" | "unmetered" | "perDayUsd" | "proactive">;

export function grantView(status: ApiCreditGrantStatus | null | undefined): GrantView {
  return status ?? { state: "unknown", budgetUsd: null, usedUsd: 0, remainingUsd: null, startsAt: null, expiresAt: null, unmeteredRequests: 0, unmetered: false, perDayUsd: null, proactive: { allowed: false, blockedBy: "GRANT_NONE" } };
}

/// `expiryMode`가 `billing-estimate`면 만료 칸은 쓰지 않고 서비스가 결제일에서 계산한 추정 만료를 보낸다. `estimateConfirmedFor`는 사용자가 동의한 그 추정 만료(ms)다. 화면에 보인 추정과 지금 추정이 다르면(결제일을 고쳤거나 폴링으로 바뀜) 동의는 무효이고 다시 받는다.
export type GrantFormInput = { budget: string; startsAt: string; expiresAt: string; confirmed: boolean; expiryMode: ExpirySource; estimateConfirmedFor: number | null };
export type GrantFormError = "budget" | "start" | "expiry" | "range" | "past" | "tooLong" | "confirm" | "estimateNone" | "estimateAnnual" | "estimatePassed" | "estimateAccount" | "estimateConfirm";
export type GrantPatch = { budgetUsd: number; startsAt: number; expiresAt: number; confirmed: true; expirySource?: "billing-estimate"; estimateConfirmed?: true };

/// 결제일로 만료를 추정할 수 있는지. 서비스가 계산한 `billing`만 믿되, 저장하는 순간에 이미 지난 날짜는 다시 거른다. 지난 날짜를 다음 달로 밀어 새로 만들지 않는다.
export type EstimateView = { expiresAt: number; blockedBy: null } | { expiresAt: null; blockedBy: "NONE" | "ANNUAL" | "DATE_PASSED" | "ACCOUNT_INVALID" };
export function estimateView(billing: ApiCreditBillingStatus | null | undefined, now: number): EstimateView {
  if (!billing) return { expiresAt: null, blockedBy: "NONE" };
  if (billing.cycle === "annual") return { expiresAt: null, blockedBy: "ANNUAL" };
  if (billing.estimateBlockedBy === "ACCOUNT_INVALID") return { expiresAt: null, blockedBy: "ACCOUNT_INVALID" };
  if (billing.estimateBlockedBy === "ANNUAL") return { expiresAt: null, blockedBy: "ANNUAL" };
  const at = billing.estimatedExpiresAt;
  if (billing.estimateBlockedBy === "DATE_PASSED" || at === null || at === undefined || at <= now) return { expiresAt: null, blockedBy: "DATE_PASSED" };
  return { expiresAt: at, blockedBy: null };
}

/// `<input type="datetime-local">` 값(시간대 없는 로컬 시각)을 ms로. 비었거나 읽을 수 없으면 null.
export function localInputToMs(value: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}/.test(value)) return null;
  const ms = new Date(value).getTime();
  return Number.isFinite(ms) ? ms : null;
}

/// ms를 `datetime-local` 값(로컬 시각, 분 단위)으로.
export function msToLocalInput(ms: number): string {
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

const ESTIMATE_ERRORS = { NONE: "estimateNone", ANNUAL: "estimateAnnual", DATE_PASSED: "estimatePassed", ACCOUNT_INVALID: "estimateAccount" } as const satisfies Record<string, GrantFormError>;

/// 서비스와 같은 금액·기간·확인 조건을 화면에서 먼저 검사한다.
/// `id`는 일부러 보내지 않는다: 저장은 항상 새 입력이며 사용액은 0부터 다시 센다(예산은 지금 남은 금액이다).
/// 정확한 만료를 직접 입력하면 `expirySource`를 보내지 않는다(이전 서비스와 같은 모양). 결제일 추정은 `expirySource: "billing-estimate"`와 별도 동의를 함께 보내고, 만료는 서비스가 준 추정값 그대로다.
export function grantPatch(form: GrantFormInput, now: number, billing?: ApiCreditBillingStatus | null): { ok: true; patch: GrantPatch } | { ok: false; error: GrantFormError } {
  const budget = form.budget.trim() === "" ? NaN : Number(form.budget);
  if (!Number.isFinite(budget) || budget < 0.01 || budget > MAX_GRANT_BUDGET_USD) return { ok: false, error: "budget" };
  const startsAt = localInputToMs(form.startsAt);
  if (startsAt === null || startsAt < 0) return { ok: false, error: "start" };
  const estimate = form.expiryMode === "billing-estimate";
  let expiresAt: number | null;
  if (estimate) {
    const view = estimateView(billing, now);
    if (view.blockedBy) return { ok: false, error: ESTIMATE_ERRORS[view.blockedBy] };
    expiresAt = view.expiresAt;
  } else {
    expiresAt = localInputToMs(form.expiresAt);
    if (expiresAt === null) return { ok: false, error: "expiry" };
  }
  if (expiresAt <= now) return { ok: false, error: "past" };
  if (expiresAt <= startsAt) return { ok: false, error: "range" };
  if (expiresAt - startsAt > MAX_GRANT_DAYS * DAY_MS || startsAt > now + MAX_GRANT_DAYS * DAY_MS) return { ok: false, error: "tooLong" };
  if (!form.confirmed) return { ok: false, error: "confirm" };
  if (estimate && form.estimateConfirmedFor !== expiresAt) return { ok: false, error: "estimateConfirm" };
  const patch: GrantPatch = { budgetUsd: Math.round(budget * 100) / 100, startsAt, expiresAt, confirmed: true };
  if (estimate) { patch.expirySource = "billing-estimate"; patch.estimateConfirmed = true; }
  return { ok: true, patch };
}

/// 저장된 입력으로 폼 초기값을 만든다. 예산 칸에는 지금 남은 추정 금액을 채워 다시 확인하게 한다(없으면 저장 당시 금액).
/// 시작은 저장된 값을 그대로 둔다(아직 시작 전인 입력의 시작이 지금으로 당겨지지 않게). 확인과 추정 동의는 항상 다시 받는다.
/// 결제일 추정으로 저장한 입력은 지금도 추정이 가능할 때만 추정 방식으로 채운다. 어느 경우든 추정 만료를 정확한 날짜 칸에 옮겨 담지 않는다(추정이 정확한 값으로 둔갑하지 않게). 정확한 날짜는 사용자가 직접 입력한다.
export function grantFormFrom(grant: ApiCreditGrantPolicy | null | undefined, status: ApiCreditGrantStatus | null | undefined, now: number): GrantFormInput {
  if (!grant) return { budget: "", startsAt: msToLocalInput(now), expiresAt: "", confirmed: false, expiryMode: "exact", estimateConfirmedFor: null };
  const remaining = status?.remainingUsd ?? grant.budgetUsd;
  const base = { budget: String(Math.max(0, Math.round(remaining * 100) / 100)), startsAt: msToLocalInput(grant.startsAt), confirmed: false, estimateConfirmedFor: null };
  if (grant.expirySource !== "billing-estimate") return { ...base, expiresAt: msToLocalInput(grant.expiresAt), expiryMode: "exact" };
  return { ...base, expiresAt: "", expiryMode: estimateView(status?.billing, now).blockedBy === null ? "billing-estimate" : "exact" };
}

/// 서비스가 결제 정보를 받는 구독 계정: Anthropic 구독 계정만(API 키·다른 공급자 제외).
export function billingAccounts<T extends Pick<Account, "provider" | "tool">>(accounts: readonly T[]): T[] {
  return accounts.filter(account => account.provider === "anthropic" && account.tool === "claude");
}

/// 서비스가 받는 결제일 상한: 월 35일, 연 367일(서비스 검증과 같다).
export const MAX_MONTHLY_BILLING_DAYS = 35;
export const MAX_ANNUAL_BILLING_DAYS = 367;
export type BillingFormInput = { accountId: string; cycle: BillingCycle; nextBillingAt: string };
export type BillingFormError = "account" | "date" | "past" | "tooFar";

/// 저장된 결제 정보로 폼 초기값을 만든다. 없으면 월 결제·빈 날짜이고, 선택할 구독 계정이 하나뿐이면 그 계정을 고른다.
export function billingFormFrom(billing: ApiCreditBillingPolicy | null | undefined, eligible: readonly Pick<Account, "id">[]): BillingFormInput {
  if (billing) return { accountId: billing.accountId, cycle: billing.cycle, nextBillingAt: msToLocalInput(billing.nextBillingAt) };
  return { accountId: eligible.length === 1 ? eligible[0].id : "", cycle: "monthly", nextBillingAt: "" };
}

/// 결제 정보 저장 요청을 만든다. 날짜 없이 날만 받아 하루 끝으로 짐작하지 않고, 입력한 시각 그대로 보낸다(시각은 이 기기의 시간대).
export function billingPatch(form: BillingFormInput, eligible: readonly Pick<Account, "id">[], now: number): { ok: true; patch: { accountId: string; cycle: BillingCycle; nextBillingAt: number } } | { ok: false; error: BillingFormError } {
  if (!eligible.some(account => account.id === form.accountId)) return { ok: false, error: "account" };
  const nextBillingAt = localInputToMs(form.nextBillingAt);
  if (nextBillingAt === null) return { ok: false, error: "date" };
  if (nextBillingAt <= now) return { ok: false, error: "past" };
  if (nextBillingAt > now + (form.cycle === "annual" ? MAX_ANNUAL_BILLING_DAYS : MAX_MONTHLY_BILLING_DAYS) * DAY_MS) return { ok: false, error: "tooFar" };
  return { ok: true, patch: { accountId: form.accountId, cycle: form.cycle, nextBillingAt } };
}

const BLOCK_KEYS = {
  OPT_IN_OFF: "balance.block.OPT_IN_OFF",
  MODE: "balance.block.MODE",
  NO_KEY: "balance.block.NO_KEY",
  GRANT_NONE: "balance.block.GRANT_NONE",
  GRANT_PENDING: "balance.block.GRANT_PENDING",
  GRANT_EXPIRED: "balance.block.GRANT_EXPIRED",
  GRANT_EXHAUSTED: "balance.block.GRANT_EXHAUSTED",
  GRANT_UNMETERED: "balance.block.GRANT_UNMETERED",
  MONTHLY_LIMIT: "balance.block.MONTHLY_LIMIT",
  AHEAD_OF_PACE: "balance.block.AHEAD_OF_PACE",
} as const satisfies Record<ProactiveBlock, string>;

/// 모르는 사유 코드(더 새로운 서비스)는 null이라 호출하는 쪽이 일반 문구를 쓴다.
export function proactiveBlockKey(code: string | null | undefined): (typeof BLOCK_KEYS)[ProactiveBlock] | null {
  return code && code in BLOCK_KEYS ? BLOCK_KEYS[code as ProactiveBlock] : null;
}
