/// 업데이터 공개키 자리표시자. `tauri.conf.json`의 `plugins.updater.pubkey`와
/// `src-tauri/src/main.rs`의 `UPDATER_PUBKEY_PLACEHOLDER`와 같은 문자열이다.
/// 이 값이면 Rust 명령이 `enabled: false`를 돌려주고, 화면은 업데이트 UI를 그리지 않는다.
export const UPDATER_PUBKEY_PLACEHOLDER = "REPLACE_WITH_TAURI_UPDATER_PUBKEY";

/// 시작 시 한 번, 그리고 앱이 켜져 있는 동안 이 간격마다 확인한다.
export const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

/// omp 브릿지 세션을 "지금 쓰는 중"으로 보는 창. `bridge.rs` ACTIVE_MS, 종료 확인 창과 같다.
export const ACTIVE_BRIDGE_MS = 15 * 60_000;

const OCCUPIED = ["PREPARED", "STARTING", "ACTIVE", "SUSPECT", "ORPHANED"] as const;

export function updatesEnabled(pubkey: string | null | undefined): boolean {
  const value = pubkey?.trim() ?? "";
  return value.length > 0 && value !== UPDATER_PUBKEY_PLACEHOLDER;
}

/// 서명 공개키가 설정되지 않았으면 확인 버튼·배지·설치를 그리지 않는다. 오류로 보여 주지 않는다.
export function updateControlsVisible(status: { enabled: boolean } | null | undefined): boolean {
  return status?.enabled === true;
}

export function updateBadgeVisible(status: { enabled: boolean; available: { version: string } | null } | null | undefined): boolean {
  return updateControlsVisible(status) && Boolean(status?.available?.version);
}

export interface InterruptCounts {
  managed: number;
  bridge: number;
  warn: boolean;
}

/// 설치 전에 읽는 세션 수. 관리 세션은 용량을 잡는 상태만, 브릿지 세션은 최근 15분 요청만.
export function updateInterruptCounts(
  sessions: { state: string }[],
  bridgeSessions: { lastUsedAt: number }[],
  now: number,
): InterruptCounts {
  const managed = sessions.filter(session => (OCCUPIED as readonly string[]).includes(session.state)).length;
  const bridge = bridgeSessions.filter(session => now - session.lastUsedAt < ACTIVE_BRIDGE_MS).length;
  return { managed, bridge, warn: managed + bridge > 0 };
}

/// 서비스 다시 시작 전에 먼저 경고할지. 쓰는 중인 관리 세션은 서비스가 직접 거절하므로 여기서는 서비스가 모르는
/// omp 브릿지 사용(최근 15분)이나 확인 실패만 경고한다.
export function restartNeedsWarning(counts: InterruptCounts, unknown: boolean): boolean {
  return unknown || counts.bridge > 0;
}
