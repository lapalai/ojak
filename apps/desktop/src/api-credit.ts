import { t } from "./i18n.ts";
import type { MessageKey } from "./i18n.ts";
import { usdLimit, usdUsed } from "./limits.ts";
import type { Account, AccountQuotaSummary, ApiCreditState, ApiKeyProvider, Policy, Snapshot } from "./types.ts";

/// API 키 계정의 tool 값(`crates/adapters/src/api_credit.rs` API_TOOL). 구독 계정이 아니다. Claude Console(anthropic)과 OpenAI Platform(openai) 키가 모두 이 값이고 `provider`로 나눈다.
export const API_TOOL = "api";
/// 월 상한 기본값(USD). `crates/protocol`의 `default_api_credit_monthly_limit_usd`와 같다.
export const DEFAULT_API_CREDIT_LIMIT_USD = 200;
/// OpenAI API 월 상한 기본값(USD). `crates/protocol`의 `default_openai_api_monthly_limit_usd`와 같다.
export const DEFAULT_OPENAI_API_LIMIT_USD = 50;
/// 입력 오타(자릿수)를 거르는 상한. 이 값 자체에 의미는 없다.
export const MAX_API_CREDIT_LIMIT_USD = 1_000_000;
/// OpenAI 월 상한 입력 범위. 서비스 검증(1~100000)과 같다.
export const MIN_OPENAI_API_LIMIT_USD = 1;
export const MAX_OPENAI_API_LIMIT_USD = 100_000;

type ApiCreditSummaryValue = NonNullable<AccountQuotaSummary["apiCredit"]>;
/// 공급자를 볼 수 있는 계정. `provider`가 없는 값(이전 기록·테스트)은 Claude로 본다.
type ProviderAccount = Pick<Account, "tool"> & { provider?: string };

export const API_KEY_PROVIDERS: readonly ApiKeyProvider[] = ["anthropic", "openai"];

export function isApiAccount(account: Pick<Account, "tool">): boolean {
  return account.tool === API_TOOL;
}

/// API 키 계정이 어느 공급자 것인지. `openai`만 OpenAI이고, 나머지(없음 포함)는 Claude다.
export function apiKeyProvider(account: { provider?: string }): ApiKeyProvider {
  return account.provider === "openai" ? "openai" : "anthropic";
}

/// 요약이 어느 공급자의 API 키 묶음인지. `apiCredit.provider`가 없으면 Claude다.
export function summaryApiProvider(summary: Pick<AccountQuotaSummary, "apiCredit"> | undefined): ApiKeyProvider {
  return summary?.apiCredit?.provider === "openai" ? "openai" : "anthropic";
}

/// 구독 계정만. 묶음·고정·동시 세션처럼 구독 계정을 전제로 하는 목록은 이 결과로 만든다.
export function subscriptionAccounts<T extends Pick<Account, "tool">>(accounts: readonly T[]): T[] {
  return accounts.filter(account => !isApiAccount(account));
}

/// 등록한 API 키 계정만. `provider`를 주면 그 공급자의 키만, 생략하면 모든 공급자의 키(목록 표시용)다. 합계를 낼 때는 항상 공급자를 지정한다.
export function apiAccounts<T extends ProviderAccount>(accounts: readonly T[], provider?: ApiKeyProvider): T[] {
  return accounts.filter(account => isApiAccount(account) && (provider === undefined || apiKeyProvider(account) === provider));
}

/// 서비스가 쓰는 집계 기간(UTC 달력월)의 시작 시각(ms). 청구 주기가 아니라 근사다.
export function utcPeriodStart(now: number): number {
  const date = new Date(now);
  return Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), 1);
}

export interface ApiCreditUsage { usedUsd: number; unmeteredRequests: number }

/// 이번 달 추정 사용액. 다른 기간에 기록된 값은 새 달이 시작됐으므로 0으로 본다.
export function apiAccountUsage(account: Pick<Account, "apiCredit">, now: number): ApiCreditUsage {
  const state = account.apiCredit;
  if (!state || state.periodStart !== utcPeriodStart(now)) return { usedUsd: 0, unmeteredRequests: 0 };
  return { usedUsd: state.usedUsd, unmeteredRequests: state.unmeteredRequests };
}

/// 등록한 키의 이번 달 합계에 지운 키의 이번 달 원장(`removed`)을 더한다. Claude는 참고용 합계, OpenAI는 합산 상한 판정용이다.
/// 공급자마다 따로 센다: Claude는 스냅샷 `apiCreditRemoved`, OpenAI는 `openaiApiRemoved`를 넘긴다. 다른 공급자의 키는 더하지 않는다(기본은 Claude).
export function apiCreditUsage(accounts: readonly (Pick<Account, "tool" | "apiCredit"> & { provider?: string })[], now: number, removed?: Pick<ApiCreditState, "periodStart" | "usedUsd" | "unmeteredRequests"> | null, provider: ApiKeyProvider = "anthropic"): ApiCreditUsage {
  const ledger = removed && removed.periodStart === utcPeriodStart(now) ? { usedUsd: removed.usedUsd, unmeteredRequests: removed.unmeteredRequests } : { usedUsd: 0, unmeteredRequests: 0 };
  return apiAccounts(accounts, provider).reduce<ApiCreditUsage>((total, account) => {
    const usage = apiAccountUsage(account, now);
    return { usedUsd: total.usedUsd + usage.usedUsd, unmeteredRequests: total.unmeteredRequests + usage.unmeteredRequests };
  }, ledger);
}

/// `$12.40 / $200 (추정)`. 금액은 Ojak의 추정치라 항상 추정임을 붙인다. 한 키의 사용액과 그 키의 상한, 또는 OpenAI 키 전체의 합계와 합계 상한에 쓴다.
export function apiCreditAmount(usedUsd: number, limitUsd: number): string {
  return t("usage.apiCredit.amount", { used: usdUsed(usedUsd), limit: usdLimit(limitUsd) });
}

/// `$12.40 (추정)`. 상한 없이 사용액만 보여 줄 때. Claude 키 전체의 합계는 상한이 걸리지 않는 보고용 값이라 이것으로만 보인다.
export function apiCreditSpend(usedUsd: number): string {
  return t("usage.apiCredit.spend", { used: usdUsed(usedUsd) });
}

/// Claude API 키 하나의 이번 달 사용액·상한 도달 여부. 상한(`apiCreditMonthlyLimitUsd`)은 키마다 따로 적용하므로 다른 키·지운 키의 사용액은 보지 않는다.
/// 서비스 배정 기준(`api_key_under_cap`: 사용액 < 상한)과 같다. 이 판정은 표시용이며 배정은 서비스가 한다.
export function apiKeyCap(account: Pick<Account, "apiCredit">, limitUsd: number, now: number): ApiCreditUsage & { limitUsd: number; reached: boolean } {
  const usage = apiAccountUsage(account, now);
  return { ...usage, limitUsd, reached: !(usage.usedUsd < limitUsd) };
}

/// 요약에서 지금 말해야 할 API 문구. 지금 쓰는 중이면 `API 크레딧 사용 중 · $150.00 / $200 (추정)`(OpenAI는 `OpenAI API 사용 중 · $12.40 / $50 (추정)`), 상한에 닿았으면 그 사실. 아니면 null.
/// Claude 요약의 금액·상한은 그 요약(키 묶음)의 사용액과 키당 상한이다. 상한은 키마다 따로 걸리므로 다른 키의 사용액을 더해 상한으로 나누지 않는다. 키가 여럿일 때의 한 줄은 `apiCreditOverview`가 만든다.
/// OpenAI 상한은 같은 공급자로 등록한 **모든 키의 합계**에 걸리므로(서비스 배정 기준) 키가 여럿이면 `totalUsedUsd`로 그 합계를 넘긴다.
export function apiCreditLabel(summary: Pick<AccountQuotaSummary, "kind" | "apiCredit"> | undefined, totalUsedUsd?: number): string | null {
  const credit: ApiCreditSummaryValue | null | undefined = summary?.apiCredit;
  if (!credit) return null;
  const openai = summaryApiProvider(summary) === "openai";
  const amount = apiCreditAmount(openai ? totalUsedUsd ?? credit.usedUsd : credit.usedUsd, credit.limitUsd);
  if (credit.limitReached) return `${t(openai ? "usage.openaiApi.capped" : "usage.apiCredit.capped")} · ${amount}`;
  if (summary?.kind === "api-credit" || credit.active) return `${t(openai ? "usage.verdict.openai-api" : "usage.verdict.api-credit")} · ${amount}`;
  return null;
}

/// 지금 새 요청이 API 키로 가고 있는 요약(있으면). 팝오버·사용 현황의 한 줄 표시에 쓴다.
export function activeApiCredit(summaries: readonly AccountQuotaSummary[] | undefined): AccountQuotaSummary | undefined {
  return summaries?.find(summary => summary.kind === "api-credit" && summary.apiCredit?.active);
}

/// 한 공급자의 키가 여럿일 때 한 줄로 말할 API 상태. 다른 공급자의 키·요약은 섞지 않는다.
/// OpenAI: 지금 배정 중인 묶음이 있으면 그것을, 없으면 묶음 하나를 고르고 금액은 모든 키의 합계다(상한도 합계 기준).
/// Claude: 상한이 키마다 따로 걸리므로 합계를 상한으로 나누지 않는다. 지금 배정 중인 키가 있으면 그중 이번 달 사용액이 가장 적은 키(서비스가 먼저 쓰는 키)의 사용액과 상한을 말하고,
/// 없으면 쓸 수 있는 키가 **모두** 상한에 닿았을 때만 상한 도달을 말한다. 키 하나만 상한에 닿고 다른 키가 남았으면 상한 도달을 말하지 않는다.
export function apiCreditOverview(accounts: readonly (Pick<Account, "id" | "tool" | "apiCredit"> & { provider?: string })[], summaries: readonly AccountQuotaSummary[] | undefined, now: number, removed?: Pick<ApiCreditState, "periodStart" | "usedUsd" | "unmeteredRequests"> | null, provider: ApiKeyProvider = "anthropic"): { label: string | null; active: boolean } {
  const ids = new Set(apiAccounts(accounts, provider).map(account => account.id));
  const mine = summaries?.filter(summary => summaryApiProvider(summary) === provider && summary.accountIds.some(id => ids.has(id))) ?? [];
  if (provider === "openai") {
    const summary = mine.find(item => item.kind === "api-credit" || item.apiCredit?.active) ?? mine[0];
    return { label: apiCreditLabel(summary, apiCreditUsage(accounts, now, removed, provider).usedUsd), active: summary?.kind === "api-credit" };
  }
  const used = (summary: AccountQuotaSummary) => summary.apiCredit?.usedUsd ?? 0;
  const active = mine.filter(item => item.kind === "api-credit" || item.apiCredit?.active).sort((a, b) => used(a) - used(b))[0];
  if (active) return { label: apiCreditLabel(active), active: active.kind === "api-credit" };
  const usable = mine.filter(item => item.kind !== "excluded");
  const capped = usable.length > 0 && usable.every(item => item.apiCredit?.limitReached) ? usable.sort((a, b) => used(b) - used(a))[0] : undefined;
  return { label: apiCreditLabel(capped), active: false };
}

/// 공급자별 이번 달 합계와 월 상한. 합계는 그 공급자의 키와 지운 키 원장만 더한다(Claude 키와 OpenAI 키를 섞지 않는다).
/// `limitUsd`의 뜻은 공급자마다 다르다: OpenAI는 모든 키 합계의 상한이고, Claude(`apiCreditMonthlyLimitUsd`)는 **키 하나당** 상한이라 `usedUsd` 합계에 걸리지 않는다(Claude 합계는 참고용 보고).
export function apiKeyTotals(snapshot: Pick<Snapshot, "accounts" | "generatedAt" | "apiCreditRemoved" | "openaiApiRemoved" | "policy">): Record<ApiKeyProvider, ApiCreditUsage & { limitUsd: number; keys: number }> {
  const { accounts, generatedAt, policy } = snapshot;
  return {
    anthropic: { ...apiCreditUsage(accounts, generatedAt, snapshot.apiCreditRemoved, "anthropic"), limitUsd: policy.apiCreditMonthlyLimitUsd ?? DEFAULT_API_CREDIT_LIMIT_USD, keys: apiAccounts(accounts, "anthropic").length },
    openai: { ...apiCreditUsage(accounts, generatedAt, snapshot.openaiApiRemoved, "openai"), limitUsd: policy.openaiApiMonthlyLimitUsd ?? DEFAULT_OPENAI_API_LIMIT_USD, keys: apiAccounts(accounts, "openai").length },
  };
}

/// 연결 화면에서 키를 추가할 때 `account.apiKeyAdd`로 보내는 값. Claude는 `provider`를 생략해 이전 서비스와 같은 모양을 유지하고, OpenAI만 명시한다.
export function apiKeyAddParams(input: { provider: ApiKeyProvider; label: string; key: string; confirmed: boolean }): { label: string; key: string; confirmedNoAutoReload: boolean; provider?: ApiKeyProvider } {
  const base = { label: input.label.trim(), key: input.key.trim(), confirmedNoAutoReload: input.confirmed };
  return input.provider === "openai" ? { ...base, provider: "openai" } : base;
}

/// 팝오버·사용 현황의 한 줄 안내 하나. 공급자마다 따로 만든다.
export interface ApiCreditNote { provider: ApiKeyProvider; label: string; active: boolean; reasonKey: MessageKey }

/// 공급자별 API 한 줄 안내. 지금 쓰는 중이거나 상한에 닿은 공급자만 나온다. 공급자마다 자기 키·요약·지운 키 원장(Claude `apiCreditRemoved`, OpenAI `openaiApiRemoved`)만 쓴다.
export function apiCreditNotes(snapshot: Pick<Snapshot, "accounts" | "quotaSummaries" | "generatedAt" | "apiCreditRemoved" | "openaiApiRemoved">): ApiCreditNote[] {
  const notes: ApiCreditNote[] = [];
  for (const provider of API_KEY_PROVIDERS) {
    const removed = provider === "openai" ? snapshot.openaiApiRemoved : snapshot.apiCreditRemoved;
    const { label, active } = apiCreditOverview(snapshot.accounts, snapshot.quotaSummaries, snapshot.generatedAt, removed, provider);
    if (label) notes.push({ provider, label, active, reasonKey: provider === "openai" ? "usage.reason.openai-api" : "usage.reason.api-credit" });
  }
  return notes;
}

export interface PaidForm { credits: boolean; extra: boolean; apiEnabled: boolean; apiLimit: string; openaiEnabled: boolean; openaiLimit: string }

/// 크레딧 설정 대화상자의 저장 값. Claude(`useApiCreditAfterLimit`·`apiCreditMonthlyLimitUsd`)와 OpenAI(`useOpenaiApiAfterLimit`·`openaiApiMonthlyLimitUsd`)는 서로 독립이다.
/// 각 API 항목은 바꾼 경우에만 담는다. 그 항목을 모르는 이전 서비스에 기존 항목 저장이 막히지 않게 한다.
export function paidPolicyPatch(base: Policy, form: PaidForm): { apiLimitValid: boolean; openaiLimitValid: boolean; changed: boolean; patch: Record<string, unknown> } {
  const apiBase = base.apiCreditMonthlyLimitUsd ?? DEFAULT_API_CREDIT_LIMIT_USD;
  const apiValue = Number(form.apiLimit);
  const apiLimitValid = form.apiLimit.trim() !== "" && Number.isFinite(apiValue) && apiValue > 0 && apiValue <= MAX_API_CREDIT_LIMIT_USD;
  const apiChanged = form.apiEnabled !== Boolean(base.useApiCreditAfterLimit) || (apiLimitValid && apiValue !== apiBase);
  const openaiBase = base.openaiApiMonthlyLimitUsd ?? DEFAULT_OPENAI_API_LIMIT_USD;
  const openaiValue = Number(form.openaiLimit);
  const openaiLimitValid = form.openaiLimit.trim() !== "" && Number.isFinite(openaiValue) && openaiValue >= MIN_OPENAI_API_LIMIT_USD && openaiValue <= MAX_OPENAI_API_LIMIT_USD;
  const openaiChanged = form.openaiEnabled !== Boolean(base.useOpenaiApiAfterLimit) || (openaiLimitValid && openaiValue !== openaiBase);
  const patch: Record<string, unknown> = { useCreditsAfterLimit: form.credits, useExtraUsageAfterLimit: form.extra };
  // 상한 입력이 올바르지 않으면 그 값은 보내지 않는다(저장 버튼도 막힌다).
  if (apiChanged) { patch.useApiCreditAfterLimit = form.apiEnabled; if (apiLimitValid) patch.apiCreditMonthlyLimitUsd = apiValue; }
  if (openaiChanged) { patch.useOpenaiApiAfterLimit = form.openaiEnabled; if (openaiLimitValid) patch.openaiApiMonthlyLimitUsd = openaiValue; }
  const changed = form.credits !== Boolean(base.useCreditsAfterLimit) || form.extra !== Boolean(base.useExtraUsageAfterLimit) || apiChanged || openaiChanged;
  return { apiLimitValid, openaiLimitValid, changed, patch };
}
