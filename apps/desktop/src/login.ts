import { errorCodeText } from "./i18n.ts";
import { loginTarget } from "./overview.ts";
import type { Account, ApiError } from "./types.ts";

/// 앱이 직접 여는 공식 로그인 작업(네이티브 `start_provider_login`)의 순수 규칙. 화면 문구는 LoginPanel이 번역한다.

export type LoginProvider = "anthropic" | "openai-codex" | "xai-oauth" | "google-antigravity";
export type LoginState = "starting" | "waiting" | "verifying" | "syncing" | "succeeded" | "failed" | "canceled";

export interface LoginIdentity { label: string; workspace: string | null }
export interface LoginStatus {
  id: string;
  provider: LoginProvider;
  /// 시작할 때 지정한 다시 로그인 대상 계정. 새 계정이면 null. 네이티브가 항상 보낸다.
  targetAccountId: string | null;
  /// 성공했을 때만 값이 있다. 실제로 확인한 Ojak 계정(다시 로그인이면 대상 계정) ID.
  accountId: string | null;
  state: LoginState;
  identity: LoginIdentity | null;
  error: ApiError | null;
  startedAt: number;
  updatedAt: number;
}

/// 로그인 한 번을 시작하는 데 필요한 값. `accountName`은 화면 표시에만 쓰고 네이티브로 보내지 않는다.
export interface LoginRequest {
  provider: LoginProvider;
  /// 다시 로그인할 계정. 비우면 새 계정이다.
  accountId?: string;
  /// 새 Claude·Codex 계정의 이름. 다른 공급자는 쓰지 않는다.
  label?: string;
  settingsDigest?: string | null;
  accountName?: string | null;
  /// 다시 로그인할 때 기대하는 작업공간(조직). 브라우저에서 같은 곳을 고르도록 안내한다.
  workspace?: string | null;
}

const PROVIDERS: readonly string[] = ["anthropic", "openai-codex", "xai-oauth", "google-antigravity"];
const STATES: readonly string[] = ["starting", "waiting", "verifying", "syncing", "succeeded", "failed", "canceled"];

export function isLoginProvider(value: unknown): value is LoginProvider {
  return typeof value === "string" && PROVIDERS.includes(value);
}

export function isTerminalLogin(state: LoginState): boolean {
  return state === "succeeded" || state === "failed" || state === "canceled";
}

/// 계정 저장소의 provider·tool 값을 공식 로그인 공급자로 바꾼다. 지원하지 않는 공급자(Z.AI 등)는 null이다.
export function loginProviderOf(account: Pick<Account, "tool" | "provider">): LoginProvider | null {
  if (account.tool === "claude") return "anthropic";
  if (account.tool === "codex") return "openai-codex";
  switch (account.provider) {
    case "anthropic": return "anthropic";
    case "openai": return "openai-codex";
    case "xai": return "xai-oauth";
    case "google": return "google-antigravity";
    default: return null;
  }
}

/// Claude·Codex 새 계정은 이름이 필요하다. xAI·Google은 새 계정도 이름이 없다.
export function needsLabel(request: Pick<LoginRequest, "provider" | "accountId">): boolean {
  return !request.accountId && (request.provider === "anthropic" || request.provider === "openai-codex");
}

export function canStartLogin(request: LoginRequest): boolean {
  return !needsLabel(request) || Boolean(request.label?.trim());
}

/// 네이티브 명령에 보낼 평평한 인자. 비어 있는 값은 보내지 않는다.
export function startArgs(request: LoginRequest): Record<string, unknown> {
  const args: Record<string, unknown> = { provider: request.provider };
  if (request.accountId) args.accountId = request.accountId;
  else if (request.label?.trim() && needsLabel(request)) {
    args.label = request.label.trim();
    if (request.settingsDigest) args.settingsDigest = request.settingsDigest;
  }
  return args;
}

/// 같은 공급자·같은 대상 계정(새 계정이면 `null`끼리)의 작업인지. 상태는 보지 않는다. 다시 로그인 대상과 새 계정, 다른 공급자는 서로 섞이지 않는다.
export function matchesRequest(status: Pick<LoginStatus, "provider" | "targetAccountId">, request: Pick<LoginRequest, "provider" | "accountId">): boolean {
  return status.provider === request.provider && status.targetAccountId === (request.accountId ?? null);
}

function newer(a: LoginStatus, b: LoginStatus): boolean {
  return a.startedAt !== b.startedAt ? a.startedAt > b.startedAt : a.updatedAt >= b.updatedAt;
}

/// 창을 다시 열 때 이어 붙일 작업. 같은 공급자·대상 계정 중
/// 1. 진행 중인 작업이 있으면 가장 최근 것(네이티브는 공급자당 하나만 허용한다),
/// 2. 없으면 끝난 작업 중 가장 최근 것 — 성공·실패·취소 어느 쪽이든 최신 작업의 결과를 그대로 보인다(오래된 성공이 새 실패를, 오래된 실패가 새 성공을 덮지 않는다).
///    그 최신 결과를 사용자가 이미 봤으면(`acknowledged`) null이다. 더 오래된 못 본 결과는 되살리지 않는다.
/// 못 본 결과는 앱 프로세스가 살아 있는 동안 계속 복원된다. 이미 본 결과가 시작 화면을 막지 않으므로 같은 공급자의 새 계정을 일부러 추가할 수 있다.
export function findLoginJob(list: readonly LoginStatus[], request: Pick<LoginRequest, "provider" | "accountId">, acknowledged: ReadonlySet<string>): LoginStatus | null {
  let active: LoginStatus | null = null;
  let finished: LoginStatus | null = null;
  for (const item of list) {
    if (!matchesRequest(item, request)) continue;
    if (!isTerminalLogin(item.state)) {
      if (!active || newer(item, active)) active = item;
    } else if (!finished || newer(item, finished)) finished = item;
  }
  if (active) return active;
  return finished && !acknowledged.has(finished.id) ? finished : null;
}

/// 다시 로그인을 시작할 수 있는 구성원에서 요청을 만든다. 공식 CLI 계정이 우선이고, 없으면 omp가 만료를 보고한 계정이다.
/// omp 계정이 Claude·Codex 것이면 앱이 공식 CLI로 로그인하므로 그 CLI가 설치돼 있어야 한다. xAI·Google은 omp의 공식 로그인을 쓴다.
export function reloginRequest(members: readonly Account[], installed: (tool: string) => boolean): LoginRequest | null {
  const target = loginTarget(members, installed) ?? members.find(member => {
    if (member.tool !== "omp" || member.authStatus !== "auth-required") return false;
    const provider = loginProviderOf(member);
    return provider !== null && (provider === "anthropic" ? installed("claude") : provider === "openai-codex" ? installed("codex") : true);
  }) ?? null;
  const provider = target ? loginProviderOf(target) : null;
  if (!target || !provider) return null;
  return { provider, accountId: target.id, accountName: target.email ?? target.label, workspace: target.organization };
}

function safeText(value: unknown, limit: number): string {
  return String(value)
    .replace(/https?:\/\/\S+/gi, "[hidden]")
    .replace(/(?:Bearer\s+\S+|(?:sk-|ghp_|xox[baprs]-)[A-Za-z0-9_-]+)/gi, "[hidden]")
    .replace(/((?:access[_-]?token|refresh[_-]?token|api[_-]?key|cookie|authorization|code|state)\s*[=:]\s*)[^\s,;&]+/gi, "$1[hidden]")
    .split(/\n\s*at\s/)[0]
    .slice(0, limit);
}

/// 네이티브 응답을 검증한다. 모양이 틀리면 null. 성공인데 확인된 계정 ID가 없으면 성공으로 보이지 않고 확인 실패로 낮춘다.
export function parseLoginStatus(raw: unknown): LoginStatus | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Record<string, unknown>;
  if (typeof value.id !== "string" || !value.id || !isLoginProvider(value.provider) || typeof value.state !== "string" || !STATES.includes(value.state)) return null;
  const state = value.state as LoginState;
  if (value.targetAccountId !== null && (typeof value.targetAccountId !== "string" || !value.targetAccountId)) return null;
  const accountId = typeof value.accountId === "string" && value.accountId ? value.accountId : null;
  const identityRaw = value.identity && typeof value.identity === "object" ? value.identity as Record<string, unknown> : null;
  const identity: LoginIdentity | null = identityRaw && typeof identityRaw.label === "string" && identityRaw.label
    ? { label: safeText(identityRaw.label, 200), workspace: typeof identityRaw.workspace === "string" && identityRaw.workspace ? safeText(identityRaw.workspace, 200) : null }
    : null;
  const errorRaw = value.error && typeof value.error === "object" ? value.error as Record<string, unknown> : null;
  let error: ApiError | null = errorRaw
    ? {
      code: typeof errorRaw.code === "string" && /^[A-Z0-9_]{1,64}$/.test(errorRaw.code) ? errorRaw.code : "LOGIN_FAILED",
      message: typeof errorRaw.message === "string" ? safeText(errorRaw.message, 500) : "",
      retryable: errorRaw.retryable === true,
    }
    : null;
  const base = {
    id: value.id,
    provider: value.provider,
    targetAccountId: value.targetAccountId,
    identity,
    startedAt: Number.isFinite(value.startedAt) ? Number(value.startedAt) : 0,
    updatedAt: Number.isFinite(value.updatedAt) ? Number(value.updatedAt) : 0,
  };
  if (state === "succeeded" && !accountId) {
    return { ...base, accountId: null, state: "failed", error: { code: "LOGIN_UNVERIFIED", message: "", retryable: true } };
  }
  // 실패인데 이유가 없으면 일반 실패로 보인다. 성공·진행 중인 작업의 오류는 버린다.
  if (state === "failed" && !error) error = { code: "LOGIN_FAILED", message: "", retryable: true };
  return { ...base, accountId, state, error: state === "failed" ? error : null };
}

/// 폴링으로 받은 새 상태를 합친다. 다른 작업의 응답, 끝난 작업이 되살아나는 응답, 더 오래된 응답은 버린다.
export function mergeLoginStatus(prev: LoginStatus | null, next: LoginStatus): LoginStatus {
  if (!prev) return next;
  if (prev.id !== next.id || isTerminalLogin(prev.state) || next.updatedAt < prev.updatedAt) return prev;
  return next;
}

/// 진행 단계 표시 위치. 브라우저 로그인(0) → 계정 확인(1) → 한도·omp 반영(2) → 끝(3). 실패·취소는 어느 단계에서 멈췄는지 알 수 없어 null이다.
export function loginStep(state: LoginState): number | null {
  switch (state) {
    case "starting": case "waiting": return 0;
    case "verifying": return 1;
    case "syncing": return 2;
    case "succeeded": return 3;
    default: return null;
  }
}

/// 계정 확인 근거 중 앱 쪽에서 "이 계정으로 실행해도 된다"고 볼 수 있는 값. 기록만 있음·설정만 확인·미확인은 아니다.
const LAUNCH_VERIFIED = ["preflight-verified", "runtime-confirmed", "upstream-confirmed"];

/// 성공 보고가 실제 스냅샷에 반영됐는지. 확인된 계정이 목록에 있어야 하고, 로그인 필요·오류 상태의 오래된 계정은 반영으로 치지 않는다.
/// 공식 CLI(claude·codex) 계정은 인증됨·실행 가능·시작 전 이상으로 확인된 계정이어야 한다.
/// omp 계정은 백엔드가 공식 OAuth와 읽기 전용 사용량을 확인한 뒤 성공을 보고하므로, 관측(미확인) 상태여도 로그인 필요·오류만 아니면 반영으로 본다.
export function reflectedInSnapshot(status: Pick<LoginStatus, "accountId">, accounts: readonly Pick<Account, "id" | "tool" | "authStatus" | "canLaunch" | "verification">[]): boolean {
  const account = status.accountId === null ? undefined : accounts.find(item => item.id === status.accountId);
  if (!account || account.authStatus === "auth-required" || account.authStatus === "error") return false;
  if (account.tool === "omp") return true;
  return account.authStatus === "authenticated" && account.canLaunch && LAUNCH_VERIFIED.includes(account.verification);
}

/// 로그인 실패를 화면 문구로 옮긴다. 계정 불일치는 실행 선택용 `ACCOUNT_MISMATCH` 문구와 뜻이 달라 로그인 전용 코드로 바꾼다.
/// 사전에 없는 코드가 설명 없이 오면 일반 로그인 실패로 보인다.
export function loginFailure(error: ApiError): ApiError {
  if (error.code === "ACCOUNT_MISMATCH") return { ...error, code: "LOGIN_ACCOUNT_MISMATCH" };
  if (!error.message.trim() && errorCodeText(error.code) === null) return { ...error, code: "LOGIN_FAILED" };
  return error;
}
