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

export interface Policy {
  revision: number;
  automatic: boolean;
  allocationMode?: "smart" | "priority";
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
  /// 서비스 실행 파일의 버전. 이 필드가 없는 이전 서비스는 알 수 없음으로 본다.
  serviceVersion?: string;
}

export interface AccountQuotaSummary {
  accountIds: string[];
  kind: "available" | "partial" | "reserve" | "resting" | "excluded" | "login" | "unknown" | "credits" | "extra";
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
