import { allocationModeOf } from "./allocation-mode.ts";
import { apiAccounts } from "./api-credit.ts";
import { t } from "./i18n.ts";
import { usdUsed } from "./limits.ts";
import type { Account, ApiCreditGrantPolicy, ApiCreditGrantState, ApiCreditGrantStatus, ApiKeyProvider, Policy, ProactiveBlock } from "./types.ts";

/// 구독 한도 뒤 유료로 넘어갈 수 있는 경로. 정책 스위치 하나가 경로 하나다.
export type PaidRoute = "codexCredits" | "claudeExtra" | "claudeApi" | "openaiApi";
type PaidPolicy = Pick<Policy, "useCreditsAfterLimit" | "useExtraUsageAfterLimit" | "useApiCreditAfterLimit" | "useOpenaiApiAfterLimit">;

/// 지금 켜져 있는 유료 경로(화면 순서 고정). 비어 있으면 Ojak은 어떤 유료 전환도 하지 않는다.
export function paidRoutes(policy: PaidPolicy): PaidRoute[] {
  const routes: PaidRoute[] = [];
  if (policy.useCreditsAfterLimit) routes.push("codexCredits");
  if (policy.useExtraUsageAfterLimit) routes.push("claudeExtra");
  if (policy.useApiCreditAfterLimit) routes.push("claudeApi");
  if (policy.useOpenaiApiAfterLimit) routes.push("openaiApi");
  return routes;
}

type GrantSource = { policy: Pick<Policy, "apiCreditGrants">; apiCreditGrants?: ApiCreditGrantStatus[] };

/// 키 하나의 크레딧 상태. `unset`은 금액·기간을 넣지 않은 키다(쓰면 그 조직 잔액·요금에서 나간다).
/// 서비스가 계산한 상태가 없으면(이전 서비스) 시각으로 알 수 있는 시작 전·만료만 정하고 나머지는 `unknown`이다. 남은 금액은 서비스 값만 쓴다.
export interface KeyCreditView {
  state: "unset" | "unknown" | ApiCreditGrantState;
  grant: ApiCreditGrantPolicy | null;
  status: ApiCreditGrantStatus | null;
  remainingUsd: number | null;
  /// 같은 입력을 쓰는 키가 여럿이다. 남은 금액은 키별이 아니라 그 입력 전체의 값이다.
  shared: boolean;
}

function grantView(grant: ApiCreditGrantPolicy, statuses: readonly ApiCreditGrantStatus[] | undefined, now: number): Omit<KeyCreditView, "grant" | "shared"> {
  const status = statuses?.find(item => item.id === grant.id) ?? null;
  if (status) return { state: status.state, status, remainingUsd: status.remainingUsd };
  const state = grant.expiresAt <= now ? "expired" : grant.startsAt > now ? "upcoming" : "unknown";
  return { state, status: null, remainingUsd: null };
}

export function keyCreditView(source: GrantSource, accountId: string, now: number): KeyCreditView {
  const grant = (source.policy.apiCreditGrants ?? []).find(item => item.accountIds.includes(accountId)) ?? null;
  if (!grant) return { state: "unset", grant: null, status: null, remainingUsd: null, shared: false };
  return { ...grantView(grant, source.apiCreditGrants, now), grant, shared: grant.accountIds.length > 1 };
}

/// 회사(공급자) 하나의 API 크레딧 요약. 금액은 입력(풀) 단위로 한 번만 더한다. 같은 입력을 쓰는 키가 여럿이어도 두 번 세지 않는다.
/// 서비스 상태가 없는 입력의 금액은 모른다. 그 금액을 0으로 더하지 않고 개수로 따로 알려, 화면이 ‘확인 안 됨’·‘확인된 것만’을 구분하게 한다.
export interface ProviderCredit {
  keys: number;
  /// 남은 금액을 아는 진행 중 입력의 합계(추정). 그런 입력이 없으면 null이다.
  leftUsd: number | null;
  /// 기간 안인데 서비스 상태가 없어 남은 금액을 모르는 입력 수. 0이 아니면 `leftUsd`는 일부 합계다.
  unknownPools: number;
  /// 기간 안인 입력(금액을 모르는 것 포함) 가운데 가장 먼저 끝나는 시각.
  nextExpiry: number | null;
  /// 만료된 입력이 끝날 때 남아 있던 금액 합계(추정). 그만큼은 쓰지 못했다.
  lostUsd: number;
  /// 만료됐지만 서비스 상태가 없어 남은 금액을 모르는 입력 수. 0이 아니면 `lostUsd`는 일부 합계다.
  lostUnknownPools: number;
  /// 배정에 포함된 키 중 금액·기간이 없는 키. 크레딧 기간을 모르므로 쓰면 그 조직 잔액·요금에서 나간다.
  unsetKeys: number;
}

export function providerCredit(source: GrantSource & { accounts: readonly Account[] }, provider: ApiKeyProvider, now: number): ProviderCredit {
  const keys = apiAccounts(source.accounts, provider);
  const ids = new Set(keys.map(key => key.id));
  const grants = (source.policy.apiCreditGrants ?? []).filter(grant => grant.accountIds.some(id => ids.has(id)));
  const credit: ProviderCredit = { keys: keys.length, leftUsd: null, unknownPools: 0, nextExpiry: null, lostUsd: 0, lostUnknownPools: 0, unsetKeys: 0 };
  for (const grant of grants) {
    const view = grantView(grant, source.apiCreditGrants, now);
    const running = view.state === "active" || view.state === "unknown";
    if (running) credit.nextExpiry = credit.nextExpiry === null ? grant.expiresAt : Math.min(credit.nextExpiry, grant.expiresAt);
    if (running && view.remainingUsd !== null) credit.leftUsd = (credit.leftUsd ?? 0) + view.remainingUsd;
    else if (running) credit.unknownPools += 1;
    else if (view.state === "expired" && view.remainingUsd !== null) credit.lostUsd += view.remainingUsd;
    else if (view.state === "expired") credit.lostUnknownPools += 1;
  }
  credit.unsetKeys = keys.filter(key => key.enabled && !grants.some(grant => grant.accountIds.includes(key.id))).length;
  return credit;
}

/// 회사 머리 아래 한 줄의 문장. 금액을 모르는 입력이 있으면 합계를 전체 잔액처럼 말하지 않는다.
/// `main`의 뒤에 붙일 만료 시각은 화면이 그린다(`showExpiry`).
export function providerCreditText(credit: ProviderCredit): { main: string; showExpiry: boolean; warnings: string[] } {
  const main = credit.leftUsd === null
    ? credit.unknownPools > 0 ? t("usage.credit.unknown") : t("usage.credit.none")
    : credit.unknownPools > 0 ? t("usage.credit.leftPartial", { amount: usdUsed(credit.leftUsd), count: credit.unknownPools }) : t("usage.credit.left", { amount: usdUsed(credit.leftUsd) });
  const warnings: string[] = [];
  if (credit.lostUsd > 0) warnings.push(t(credit.lostUnknownPools > 0 ? "usage.credit.lostPartial" : "usage.credit.lost", { amount: usdUsed(credit.lostUsd), count: credit.lostUnknownPools }));
  else if (credit.lostUnknownPools > 0) warnings.push(t("usage.credit.lostUnknown", { count: credit.lostUnknownPools }));
  if (credit.unsetKeys > 0) warnings.push(t("usage.credit.unset", { count: credit.unsetKeys }));
  return { main, showExpiry: credit.nextExpiry !== null, warnings };
}

/// ‘만료 전에 API 크레딧 쓰기’가 지금 이 공급자에서 실제로 동작하는지. 서비스가 계산한 입력별 판정을 믿고, 그 판정이 없는 이유(키·입력 없음)만 화면에서 정한다.
/// `PROVIDER_OFF`: 이 공급자의 한도 뒤 API 스위치가 꺼져 있다(서비스 코드 `OPT_IN_OFF`를 공급자별로 풀어 쓴 것).
/// `waiting`: 막힌 것이 아니라 크레딧이 일정보다 앞서 있어 아직 미리 쓸 필요가 없다.
export type BalanceBlock = ProactiveBlock | "PROVIDER_OFF" | "UNKNOWN";
export type BalanceStatus = { allowed: true } | { allowed: false; reason: BalanceBlock; waiting: boolean };

export function balanceStatus(source: GrantSource & { accounts: readonly Account[]; policy: Pick<Policy, "apiCreditGrants" | "allocationMode" | "balanceApiCredit" | "useApiCreditAfterLimit" | "useOpenaiApiAfterLimit"> }, provider: ApiKeyProvider): BalanceStatus | null {
  const keys = apiAccounts(source.accounts, provider);
  if (keys.length === 0) return null;
  const { policy } = source;
  const blocked = (reason: BalanceBlock): BalanceStatus => ({ allowed: false, reason, waiting: reason === "AHEAD_OF_PACE" });
  if (!policy.balanceApiCredit) return blocked("OPT_IN_OFF");
  if (allocationModeOf(policy.allocationMode) !== "expiry-balanced") return blocked("MODE");
  if (!(provider === "openai" ? policy.useOpenaiApiAfterLimit : policy.useApiCreditAfterLimit)) return blocked("PROVIDER_OFF");
  const ids = new Set(keys.map(key => key.id));
  const grants = (policy.apiCreditGrants ?? []).filter(grant => grant.accountIds.some(id => ids.has(id)));
  if (grants.length === 0) return blocked("GRANT_NONE");
  if (!source.apiCreditGrants) return blocked("UNKNOWN");
  const statuses = grants.map(grant => source.apiCreditGrants!.find(item => item.id === grant.id)).filter((status): status is ApiCreditGrantStatus => status !== undefined);
  if (statuses.some(status => status.proactive.allowed)) return { allowed: true };
  // 진행 중인 입력이 있으면 그 이유가 지금 의미 있는 이유다. 만료된 옛 입력의 이유로 덮지 않는다.
  const lead = statuses.find(status => status.state === "active") ?? statuses[0];
  const reason = lead?.proactive.blockedBy ?? "UNKNOWN";
  return blocked(reason === "OPT_IN_OFF" ? "PROVIDER_OFF" : reason);
}
