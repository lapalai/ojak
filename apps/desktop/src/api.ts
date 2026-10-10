import { invoke, isTauri } from "@tauri-apps/api/core";
import type { ApiError, HostConnections, LaunchIntent } from "./types";
import { t } from "./i18n";
import { parseLoginStatus, startArgs } from "./login";
import type { LoginRequest, LoginStatus } from "./login";

export function toApiError(value: unknown): ApiError {
  let candidate = value;
  if (typeof candidate === "string") {
    try { candidate = JSON.parse(candidate); } catch { candidate = null; }
  }
  if (candidate && typeof candidate === "object" && "code" in candidate && "message" in candidate
    && typeof candidate.code === "string" && typeof candidate.message === "string") {
    return {
      code: /^[A-Z0-9_]{1,64}$/.test(candidate.code) ? candidate.code : "REQUEST_FAILED",
      message: candidate.message
        .replace(/(?:Bearer\s+\S+|(?:sk-|ghp_|xox[baprs]-)[A-Za-z0-9_-]+)/gi, t("api.redacted"))
        .replace(/((?:access[_-]?token|refresh[_-]?token|api[_-]?key|cookie|authorization)\s*[=:]\s*)[^\s,;]+/gi, `$1${t("api.redacted")}`)
        .split(/\n\s*at\s/)[0].slice(0, 700),
      retryable: "retryable" in candidate && candidate.retryable === true,
    };
  }
  return { code: "REQUEST_FAILED", message: t("api.requestFailed"), retryable: true };
}

export async function native<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) {
    throw { code: "DESKTOP_REQUIRED", message: t("api.desktopRequired"), retryable: false } satisfies ApiError;
  }
  try { return await invoke<T>(command, args); } catch (error) { throw toApiError(error); }
}

export const rpc = <T,>(method: string, params: Record<string, unknown> = {}) => native<T>("rpc", { method, params });
export const chooseDirectory = () => native<string | null>("choose_directory");
export const hostConnections = () => native<HostConnections>("host_connections");
export const launchSession = (intent: LaunchIntent) => native<{ opened: boolean }>("launch_session", { intent });
export interface SettingsPreview {
  tool: string;
  source: string;
  digest: string;
  canImport: boolean;
  changes: { key: string; value: unknown }[];
  omitted: string[];
  warnings: string[];
}
export const settingsPreview = (tool: string) => native<SettingsPreview>("settings_preview", { tool });
/// 앱이 공식 로그인을 직접 연다. 응답은 작업 상태일 뿐이고, 성공은 `succeeded`(계정 확인 뒤)로만 알 수 있다.
const loginStatus = (raw: unknown): LoginStatus => {
  const status = parseLoginStatus(raw);
  if (!status) throw { code: "REQUEST_FAILED", message: t("api.requestFailed"), retryable: true } satisfies ApiError;
  return status;
};
export const startProviderLogin = async (request: LoginRequest) => loginStatus(await native<unknown>("start_provider_login", startArgs(request)));
export const providerLoginStatus = async (id: string) => loginStatus(await native<unknown>("provider_login_status", { id }));
export const cancelProviderLogin = async (id: string) => loginStatus(await native<unknown>("cancel_provider_login", { id }));
/// 아직 끝나지 않은 로그인 작업. 창을 다시 열었을 때 이어 붙인다.
export const listProviderLogins = async () => (await native<unknown[]>("list_provider_logins")).flatMap(raw => parseLoginStatus(raw) ?? []);
/// macOS는 입력줄에 `claude`/`codex`만 적힌 Terminal을 연다(실행은 사용자가 Enter를 칠 때). 채울 수 없는 환경은 `{ opened: false, prefilled: false }`이고 아무것도 열지 않는다.
export const openPrefilledTerminal = (tool: "claude" | "codex") => native<{ opened: boolean; prefilled: boolean }>("open_prefilled_terminal", { tool });
export const stopService = () => native<string>("stop_service");
export const exportDiagnostics = (range: { from?: number; to?: number } = {}) => native<{ saved: boolean; cancelled: boolean }>("export_diagnostics", range);
export interface OmpBrokerStatus {
  supported: boolean;
  connected: boolean;
  configPath: string;
  tokenPath: string;
  url: string;
  managedBlock: boolean;
  sourceDigest: string;
  supervised: boolean;
  accountCount?: number;
  providers: string[];
}
export const ompBrokerAction = (action: "status" | "connect" | "disconnect") => native<OmpBrokerStatus>("omp_broker_action", { action });
export interface BridgeGateway { provider: string; email: string; port: number; running: boolean }
export interface BridgeSession { session: string; provider: string; email: string; model: string; cwd: string | null; lastUsedAt: number; requests: number }
export interface OmpBridgeState {
  enabled: boolean;
  listening: boolean;
  port: number;
  error: string | null;
  gateways: BridgeGateway[];
  sessions: BridgeSession[];
  blocks: { provider: string; email: string; scope: string | null; until: number; reason: string; quota: boolean }[];
}
export interface OmpBridgeStatus {
  connected: boolean;
  extensionInstalled: boolean;
  extensionPath: string;
  url: string;
  bridge: OmpBridgeState | null;
}
export const ompBridgeAction = (action: "status" | "connect" | "disconnect") => native<OmpBridgeStatus>("omp_bridge_action", { action });
/// bridge.log에서 모은 (공급자, 모델, 계정)별 요청 시계열. `series`·`total`은 대화 요청, `auxiliary`는 judge 같은 보조 요청 수다. `series`는 오래된 구간부터 5분 단위다.
export interface UsageBucket { provider: string; model: string; account: string; series: number[]; total: number; auxiliary: number; sessions: string[] }
export interface BridgeUsage { binMinutes: number; from: number; to: number; buckets: UsageBucket[] }
export const bridgeUsage = (windowMinutes: number) => native<BridgeUsage>("bridge_usage", { windowMinutes });
export const contactAuthor = () => native<{ opened: boolean }>("contact_author", {});
export interface AppInfo { version: string; protocolVersion: number; integrationVersion: number; identifier: string; platform: string; home: string; contact: boolean }
export const appInfo = () => native<AppInfo>("app_info");
export const deactivatePlan = () => native<string>("deactivate_plan");
export const refreshIntegrations = () => native<string>("refresh_integrations");
export const getLanguage = () => native<{ choice: "system" | "en" | "ko" | "id"; resolved: "en" | "ko" | "id" }>("get_language");
export const setLanguage = (choice: "system" | "en" | "ko" | "id") => native<{ choice: string; resolved: "en" | "ko" | "id" }>("set_language", { choice });
export const getTrayThreshold = () => native<number>("get_tray_threshold");
export const setTrayThreshold = (value: number) => native<number>("set_tray_threshold", { value });
export const getExpiringNotify = () => native<boolean>("get_expiring_notify");
export const setExpiringNotify = (value: boolean) => native<boolean>("set_expiring_notify", { value });
/// 한도 회복 알림을 켜 둔 계정 ID. 같은 계정 묶음의 어느 구성원 ID든 올 수 있다.
export const getRecoveryWatches = () => native<string[]>("get_recovery_watches");
/// 켜기는 OS 알림 권한이 있어야 성공한다. 끄기는 같은 묶음 전체를 해제한다. 둘 다 갱신된 전체 ID 목록을 돌려준다.
export const setRecoveryWatch = (accountId: string, enabled: boolean) => native<string[]>("set_recovery_watch", { accountId, enabled });
export const setPrivacy = (masked: boolean) => native<void>("set_privacy", { masked });
export interface SetupNotice {
  step: string; tool: string | null; code: string; message: string; params?: Record<string, string>;
}
export interface SetupStatus {
  service: boolean; shell: boolean; configured: boolean; ready: boolean; ompDetected: boolean; ompSupported: boolean;
  tools: { tool: string; account: boolean; connected: boolean; verified: boolean | null; verificationError: string | null }[];
  notices?: SetupNotice[];
  /// 실행 중인 서비스가 앱과 버전이 다르거나 알 수 없다. 이 기능 이전 서비스는 필드가 없을 수 있다.
  serviceVersionMismatch?: boolean;
  omp?: { broker: boolean; bridge: boolean; observer: boolean; error: string | null; futureLaunchesOnly: boolean };
}
export const setupStatus = () => native<SetupStatus>("setup_status");
export const setupCheck = () => native<SetupStatus>("setup_install", { check: true, withOmp: false });
export const setupInstall = (withOmp = false) => native<SetupStatus>("setup_install", { check: false, withOmp });
export const openHomepage = () => native<{ opened: boolean }>("open_homepage");
export const getAutostart = () => native<boolean>("get_autostart");
export const setAutostart = (enabled: boolean) => native<boolean>("set_autostart", { enabled });
export const quitApp = (mode: "keep" | "deactivate") => native<string>("quit_app", { mode });
export interface UpdateOffer { version: string; notes: string }
export interface UpdateStatus { enabled: boolean; available: UpdateOffer | null; error: string | null }
export const updatesStatus = (force = false) => native<UpdateStatus>("updates_status", { force });
export const installUpdate = () => native<void>("install_update");
export const restartAfterUpdate = () => native<void>("restart_after_update");
export interface ServiceVersionReport { appVersion: string; serviceVersion: string | null; state: "current" | "older" | "newer" | "unknown"; mismatch: boolean }
/// 앱과 실행 중인 서비스의 버전 비교(읽기 전용). 서비스가 응답하지 않으면 오류.
export const serviceVersionStatus = () => native<ServiceVersionReport>("service_version_status");
/// 사용자가 눌렀을 때만 부른다. 쓰는 중인 세션이 있으면 서비스가 `SESSION_BUSY`로 거절하고 아무것도 바꾸지 않는다.
export const serviceRestart = () => native<string>("service_restart");

export function assertOpened(result: { opened: boolean }): void {
  if (!result.opened) throw { code: "TERMINAL_NOT_OPENED", message: t("api.terminalNotOpened"), retryable: true } satisfies ApiError;
}
