/// 계정 저장소의 provider 값. 화면의 공급자 보드는 이 순서로 그린다.
export const PROVIDER_ORDER = ["anthropic", "openai", "google", "xai", "other"];

export interface QuotaBucket {
  id: string;
  label: string;
  model: string | null;
  usedPercent: number | null;
  resetsAt: number | null;
  observedAt: number;
  source: string;
  status: string;
}

/// API 키 계정이 속한 공급자. `account.provider`와 같은 값(anthropic = Claude Console, openai = OpenAI Platform)이다.
export type ApiKeyProvider = "anthropic" | "openai";

/// API 키의 이번 달(UTC 달력월) 추정 사용액 원장. 두 공급자가 같은 모양이다.
export interface ApiCreditState { periodStart: number; usedUsd: number; unmeteredRequests: number; updatedAt: number }

export interface Account {
  id: string;
  provider: string;
  tool: string;
  label: string;
  email: string | null;
  organization: string | null;
  plan: string | null;
  profilePath: string | null;
  binaryPath: string | null;
  identityKey: string | null;
  authStatus: string;
  verification: string;
  canLaunch: boolean;
  ompCredentialPins?: { provider: string; hash: string }[];
  reason: string | null;
  enabled: boolean;
  maxConcurrency: number;
  buckets: QuotaBucket[];
  lastCheckedAt: number;
  /// Codex 크레딧 관측. 있으면 공급자가 명시한 값이다.
  credits?: { available: boolean; unlimited: boolean; balance: string | null; ordinaryUsageAllowed?: boolean | null; observedAt: number } | null;
  /// Claude 추가 사용량 관측(USD). 켜져 있을 때만 있다.
  extraUsage?: { enabled: boolean; usedUsd: number; limitUsd: number | null; observedAt: number } | null;
  /// API 키 계정(`tool === "api"`)의 이번 달 추정 사용액(USD). 키 값은 담지 않는다. 구독 계정에는 없다.
  apiCredit?: ApiCreditState | null;
}

export interface ToolStatus {
  id: string;
  name: string;
  provider: string;
  binaryPath: string | null;
  version: string | null;
  installed: boolean;
  isolation: string;
  reason: string | null;
}

export interface ProjectRoute {
  path: string;
  scope: "directory" | "repository";
  tool: string;
  mode: "pinned" | "automatic" | "unmanaged";
  accountId: string | null;
  model: string | null;
}

export interface HostConnections {
  shimDirectory: string;
  shims: { tool: string; path: string; installed: boolean }[];
  hosts: {
    id: string;
    name: string;
    installed: boolean;
    status: "configured" | "setup-required" | "unsupported" | "unavailable";
    /// 첫 줄이 상태 요약, 나머지는 펼쳐 보는 안내. 문장은 화면 사전(`hosts.note.<key>`)이 표시 언어로 만든다.
    notes: { key: string; params?: Record<string, string> }[];
    commands: { tool: string; command: string; setting: string }[];
  }[];
}

export type AllocationMode = "smart" | "priority" | "expiry-balanced";

/// 사용자가 확인했다고 입력한 API 크레딧. `budgetUsd`는 저장 시점에 남아 있다고 확인한 금액이고, 사용액은 `confirmedAt`부터 센다.
/// `expirySource`가 없으면(이전 입력) 사용자가 정확한 만료일을 직접 입력한 것(`exact`)이다. `billing-estimate`는 구독 결제일에서 추정한 만료이고 `billingEstimate`가 그 근거다.
export interface ApiCreditGrantPolicy {
  id: string;
  budgetUsd: number;
  startsAt: number;
  expiresAt: number;
  confirmedAt: number;
  source: "manual";
  expirySource?: ExpirySource;
  billingEstimate?: { accountId: string; nextBillingAt: number } | null;
}

export type ExpirySource = "exact" | "billing-estimate";
export type BillingCycle = "monthly" | "annual";
/// 사용자가 직접 입력한 구독 결제 주기. Anthropic에서 읽은 값이 아니다. 저장해도 크레딧 입력의 만료·사용액은 바뀌지 않는다.
export interface ApiCreditBillingPolicy {
  accountId: string;
  cycle: BillingCycle;
  nextBillingAt: number;
  updatedAt: number;
}
/// 서비스가 계산한 결제일 기반 추정. 월 결제이고 결제일이 미래일 때만 `estimatedExpiresAt`이 있다.
export type EstimateBlock = "ANNUAL" | "DATE_PASSED" | "ACCOUNT_INVALID";
export interface ApiCreditBillingStatus {
  accountId: string;
  cycle: BillingCycle;
  nextBillingAt: number;
  estimatedExpiresAt: number | null;
  estimateBlockedBy: EstimateBlock | null;
}

export type ApiCreditGrantState = "unknown" | "upcoming" | "active" | "exhausted" | "expired";
export type ProactiveBlock = "OPT_IN_OFF" | "MODE" | "NO_KEY" | "GRANT_NONE" | "GRANT_PENDING" | "GRANT_EXPIRED" | "GRANT_EXHAUSTED" | "GRANT_UNMETERED" | "MONTHLY_LIMIT" | "AHEAD_OF_PACE";

/// 서비스가 매 스냅샷마다 계산한 API 크레딧 입력값 상태. 사용액은 Ojak 추정치이며 Console 잔액이 아니다.
export interface ApiCreditGrantStatus {
  id: string | null;
  source: "manual";
  state: ApiCreditGrantState;
  budgetUsd: number | null;
  usedUsd: number;
  remainingUsd: number | null;
  startsAt: number | null;
  expiresAt: number | null;
  unmeteredRequests: number;
  /// true면 일부 요청의 사용액을 읽지 못했다. 미리 쓰기는 막힌다.
  unmetered: boolean;
  perDayUsd: number | null;
  monthlyLimitReached: boolean;
  proactive: { allowed: boolean; blockedBy: ProactiveBlock | null };
  /// 이 필드들이 없는 이전 서비스는 `exact`·근거 없음·어긋남 없음·결제 정보 없음으로 본다.
  expirySource?: ExpirySource;
  billingEstimate?: { accountId: string; nextBillingAt: number } | null;
  /// 추정 만료의 근거가 된 결제일이 지금 저장된 결제 정보와 다르다. 입력값은 바뀌지 않았다.
  estimateDrift?: boolean;
  billing?: ApiCreditBillingStatus | null;
}

export interface Policy {
  revision: number;
  automatic: boolean;
  allocationMode?: AllocationMode;
  autoTakeover?: boolean;
  accountPriority?: string[];
  /// 공급자별 수동 배정 계정 ID. 없는 공급자는 자동 배정이다.
  providerPins: Record<string, string>;
  projectAllowlist: Record<string, string[]>;
  projectRoutes: ProjectRoute[];
  safetyReservePercent: number;
  staleAfterSeconds: number;
  /// 곧 리셋될 남은 한도가 있는 계정을 스마트 배정에서 먼저 쓴다.
  expiringBoost?: boolean;
  /// 리셋까지 이 시간 이내인 주간 한도만 본다.
  expiringWindowHours?: number;
  /// 안전 여유량을 뺀 남은 한도가 이 % 이상일 때만 알린다.
  expiringMinPercent?: number;
  /// 구독 한도를 다 쓴 Codex 계정에서 크레딧(과금될 수 있음)을 쓰도록 배정한다. 기본 꺼짐.
  useCreditsAfterLimit?: boolean;
  /// 구독 한도를 다 쓴 Claude 계정에서 추가 사용량(API 요금)을 쓰도록 배정한다. 기본 꺼짐.
  useExtraUsageAfterLimit?: boolean;
  /// 모든 Claude 구독 계정이 한도를 다 쓴 뒤 등록한 API 키(Max 플랜 월 API 크레딧)로 omp·`claude -p` 요청을 보낸다. 기본 꺼짐.
  useApiCreditAfterLimit?: boolean;
  /// API 크레딧 월(UTC 달력월) 추정 사용액 상한(USD). Ojak의 보조 장치이며 실제 청구 상한이 아니다. 기본 200.
  apiCreditMonthlyLimitUsd?: number;
  /// 모든 Codex 구독 계정이 한도를 다 쓴 뒤 등록한 OpenAI API 키로 omp·비대화형 `codex exec`/`codex review` 요청을 보낸다. 실제 요금이 청구된다. Claude 설정과 별개이며 기본 꺼짐.
  useOpenaiApiAfterLimit?: boolean;
  /// OpenAI API 월(UTC 달력월) 추정 사용액 상한(USD). Ojak의 보조 장치이며 OpenAI가 강제하는 상한이 아니다. 기본 50, 1~100000.
  openaiApiMonthlyLimitUsd?: number;
  /// `expiry-balanced` 모드에서 곧 사라질 API 크레딧을 새 omp 세션에 미리 쓰도록 허용한다. 기본 꺼짐. `useApiCreditAfterLimit`도 켜져 있어야 한다.
  balanceApiCredit?: boolean;
  /// 사용자가 직접 확인해 입력한 API 크레딧 사용 가능 금액·기간(등록한 모든 키가 함께 쓰는 하나의 풀). Console 잔액을 읽은 값이 아니다. 없으면 모른다.
  apiCreditGrant?: ApiCreditGrantPolicy | null;
  /// 사용자가 직접 입력한 구독 계정의 결제 주기·다음 결제일. 월 결제일은 만료 추정에 미리 채울 수만 있고 그 자체가 만료는 아니다. 없으면 모른다.
  apiCreditBilling?: ApiCreditBillingPolicy | null;
}

export interface ProcessIdentity {
  pid: number;
  startedAt: string;
  bootId: string;
}

export interface Session {
  id: string;
  requestId: string;
  accountId: string;
  tool: string;
  model: string;
  cwd: string;
  state: string;
  verification: string;
  startedAt: number;
  updatedAt: number;
  process: ProcessIdentity | null;
  supervisor: ProcessIdentity | null;
  spawnAttemptId: string | null;
  generation: string;
  exitCode: number | null;
  reason: string | null;
  nativeSessionId: string | null;
  parentSessionId?: string | null;
  backgroundProcesses?: ProcessIdentity[] | null;
}

export interface ObservedAttribution {
  sessionId: string;
  role: string;
  provider: string;
  model: string | null;
  accountId: string | null;
  verification: string;
  recordedAt: number;
  stopReason: string | null;
  source: string;
  route?: "bridge" | "direct" | "unknown" | null;
  reason: string | null;
}

export interface ObservedSession {
  id: string;
  tool: string;
  process: ProcessIdentity;
  parentProcess: ProcessIdentity | null;
  host: "orca" | "terminal" | "unknown";
  cwd: string | null;
  model: string | null;
  accountId: string | null;
  verification: string;
  reason: string | null;
  attributions?: ObservedAttribution[];
  nativeSessionId?: string | null;
}

export interface Notice {
  id: string;
  level: string;
  title: string;
  message: string;
}

export interface Takeover {
  tool: string;
  nativeSessionId: string;
  accountId: string;
  cwd: string | null;
  adoptedAt: number;
  source: "manual" | "automatic";
  evidence: string;
}

export interface Snapshot {
  version: number;
  generatedAt: number;
  serviceStartedAt: number;
  accounts: Account[];
  tools: ToolStatus[];
  sessions: Session[];
  observedSessions?: ObservedSession[];
  policy: Policy;
  notices: Notice[];
  refreshing: boolean;
  lastRefreshAt: number | null;
  takeovers?: Takeover[];
  quotaSummaries?: AccountQuotaSummary[];
  /// 연결을 해제한 Claude API 키가 이번 달(UTC 달력월)에 쓴 추정 사용액 원장. 월 상한 합계에 더해 보여 준다. 없으면 0.
  apiCreditRemoved?: ApiCreditState | null;
  /// 연결을 해제한 OpenAI API 키의 이번 달 추정 사용액 원장. OpenAI 월 상한 합계에만 더한다. 없으면 0.
  openaiApiRemoved?: ApiCreditState | null;
  /// 직접 입력한 API 크레딧의 상태. 서비스가 항상 보내며(입력이 없으면 `state: "unknown"`), 이 필드가 없는 이전 서비스는 모르는 것으로 본다.
  apiCreditGrant?: ApiCreditGrantStatus | null;
  /// 서비스 실행 파일의 버전. 이 필드가 없는 이전 서비스는 알 수 없음으로 본다.
  serviceVersion?: string;
}

export interface AccountQuotaSummary {
  accountIds: string[];
  kind: "available" | "partial" | "reserve" | "resting" | "excluded" | "login" | "unknown" | "credits" | "extra" | "api-credit";
  until: number | null;
  models: string[];
  label: string | null;
  rate: boolean;
  /// 곧 리셋되는데 많이 남은 긴 주기 한도. 리셋 전에 쓰면 아낄 수 있다.
  expiring?: ExpiringQuota | null;
  /// Codex 크레딧(ChatGPT 크레딧 단위, 금액 아님). 신선한 관측이 명시한 경우에만 있다. `active`는 지금 구독 한도 대신 크레딧으로 배정되는 상태.
  credits?: { active: boolean; unlimited: boolean; balance: string | null } | null;
  /// Claude 추가 사용량(USD, API 요금). 신선한 관측에서 켜져 있을 때만 있다. `limitReached`면 쓸 수 없다.
  extraUsage?: { active: boolean; usedUsd: number; limitUsd: number | null; limitReached: boolean } | null;
  /// API 키(`tool === "api"`) 묶음. 금액은 Ojak 추정치(USD)다. `active`는 지금 새 요청이 이 키로 배정되는 상태. `provider`가 없으면 Claude(anthropic)다.
  apiCredit?: { active: boolean; usedUsd: number; limitUsd: number; limitReached: boolean; unmetered: boolean; provider?: ApiKeyProvider } | null;
}

export interface ExpiringQuota {
  label: string;
  resetsAt: number;
  usablePercent: number;
  perHour: number;
}

export interface LaunchIntent {
  tool: string;
  model: string;
  cwd: string;
  accountId: string | null;
  parentSessionId: string | null;
  resumeSessionId: string | null;
  nonInteractive?: boolean;
}

export interface Candidate {
  accountId: string;
  eligible: boolean;
  score: number | null;
  reasons: string[];
}

export interface Decision {
  selectedAccountId: string | null;
  candidates: Candidate[];
  policyRevision: number;
}

export interface ApiError {
  code: string;
  message: string;
  retryable: boolean;
}
