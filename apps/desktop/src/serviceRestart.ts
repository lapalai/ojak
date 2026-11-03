/// 서비스 다시 시작이 끊을 작업과, 그것을 화면에 어떻게 보여 줄지 정하는 순수 함수. 네이티브(`service_version.rs`)의 `BusyReport`·`RestartReport`와 같은 모양이다.
/// 이 파일은 런타임 import가 없다(테스트가 Tauri 없이 불러온다).

export interface ToolBusy { tool: string; active: number; uncertain: number }
/// `known`이 거짓이면 omp 응답이 진행 중인지 읽지 못한 것이다(이 값이 없던 예전 서비스 포함). 0이 아니다.
export interface OmpBusy { known: boolean; streams: number; queued: number }
export interface BusyReport { tools: ToolBusy[]; omp: OmpBusy }
export type RestartStatus = "restarted" | "unchanged" | "busy";
export interface RestartReport { status: RestartStatus; message: string; busy: BusyReport }

export type BusyItem =
  | { kind: "ompStreams"; count: number }
  | { kind: "ompQueued"; count: number }
  | { kind: "ompUnknown" }
  | { kind: "terminal"; tool: string; count: number };

/// 확인 창에 한 줄씩 보여 줄 항목. 개수가 0인 것은 뺀다.
export function busyItems(busy: BusyReport): BusyItem[] {
  const items: BusyItem[] = [];
  if (!busy.omp.known) items.push({ kind: "ompUnknown" });
  else {
    if (busy.omp.streams > 0) items.push({ kind: "ompStreams", count: busy.omp.streams });
    if (busy.omp.queued > 0) items.push({ kind: "ompQueued", count: busy.omp.queued });
  }
  for (const tool of busy.tools) {
    const count = tool.active + tool.uncertain;
    if (count > 0) items.push({ kind: "terminal", tool: tool.tool, count });
  }
  return items;
}

/// 사용자 확인 없이 다시 시작하면 안 되는지. omp를 읽지 못한 것도 쉬고 있음의 증거가 아니므로 확인을 받는다.
export function needsConsent(busy: BusyReport): boolean {
  return busyItems(busy).length > 0;
}

export interface RestartGuidance {
  /// omp 응답이 끊겼거나 끊겼을 수 있다: 보내기 전 작업은 `/ojak-resume`, 이미 보낸 요청은 결과부터 확인.
  omp: boolean;
  /// 관리 중인 Claude·Codex 터미널 대화가 있었다.
  terminals: boolean;
}

/// 강제로 다시 시작한 뒤 보여 줄 안내. 끊긴 작업이 없으면 둘 다 거짓이라 안내를 띄우지 않는다.
export function restartGuidance(busy: BusyReport): RestartGuidance {
  const items = busyItems(busy);
  return {
    omp: items.some(item => item.kind !== "terminal"),
    terminals: items.some(item => item.kind === "terminal"),
  };
}

export function toolName(tool: string): string {
  return tool === "claude" ? "Claude" : tool === "codex" ? "Codex" : tool;
}
