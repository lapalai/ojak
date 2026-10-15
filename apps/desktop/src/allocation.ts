/// 배정 준비 상태와 계정별 동시 세션 설정의 순수 판정. React·번역에 기대지 않아 `node --test`로 바로 검증한다.
/// 배정 가능 여부는 서비스의 `route.explain` 결과(`setup.tools[].allocation`)만 믿고 여기서 다시 계산하지 않는다.

export type AllocationState = "allocatable" | "pending" | "excluded" | "none" | "unavailable";

export interface ToolAllocation {
  state: AllocationState;
  candidates: number;
  eligible: number;
  /// 제외된 계정의 사유 코드. 설명 문장·계정 정보는 없다.
  codes: string[];
  errorCode?: string;
}

const STATES: readonly string[] = ["allocatable", "pending", "excluded", "none", "unavailable"];

/// 이 기능 이전 CLI는 `allocation`을 주지 않는다. 모르는 값은 '읽지 못함'으로 두어 배정 가능이라고 말하지 않는다.
export function parseAllocation(raw: unknown): ToolAllocation {
  const value = raw && typeof raw === "object" ? raw as Record<string, unknown> : {};
  const state = typeof value.state === "string" && STATES.includes(value.state) ? value.state as AllocationState : "unavailable";
  const codes = Array.isArray(value.codes) ? value.codes.filter((code): code is string => typeof code === "string" && /^[A-Z0-9_]{1,64}$/.test(code)) : [];
  return {
    state,
    candidates: Number.isInteger(value.candidates) ? value.candidates as number : 0,
    eligible: Number.isInteger(value.eligible) ? value.eligible as number : 0,
    codes,
    ...(typeof value.errorCode === "string" ? { errorCode: value.errorCode } : {}),
  };
}

/// 명령 연결(준비 단계)과 별개로, 지금 계정을 받을 수 있는 도구가 하나라도 있는지.
export function anyAllocatable(tools: readonly { allocation?: ToolAllocation }[]): boolean {
  return tools.some(tool => tool.allocation?.state === "allocatable");
}

/// 지금 새 세션을 받을 수 있다고 서비스가 판단한 도구만 남긴다. 터미널을 열어 줄 도구는 `canLaunch`가 아니라 이 판단으로 고른다.
export function allocatableTools(tools: readonly { tool: string; allocation?: ToolAllocation }[]): string[] {
  return tools.filter(tool => tool.allocation?.state === "allocatable").map(tool => tool.tool);
}

export const MAX_CONCURRENCY = 32;

interface ConcurrencyAccount {
  id: string;
  provider: string;
  tool: string;
  canLaunch: boolean;
  verification: string;
  identityKey: string | null;
  maxConcurrency: number;
  ompCredentialPins?: { provider: string; hash: string }[];
}

/// 동시 세션 수를 바꿀 수 있는 연결: 공식 확인을 마친 Claude Code 실행 연결만. Codex·관측 전용 omp 행은 넣지 않는다
/// (서비스도 같은 조건이 아니면 `ADAPTER_UNVERIFIED`로 거절한다).
export function concurrencyTargets<T extends ConcurrencyAccount>(members: readonly T[]): T[] {
  return members.filter(member => member.tool === "claude" && member.canLaunch && member.verification === "preflight-verified");
}

/// 서비스 배정이 같은 실제 계정으로 세는 기준(공급자와 확인된 identity 키가 같거나 자격 증명 pin이 겹침)과 같다.
/// 화면의 계정 묶음은 같은 워크스페이스·사용자만 같아도 합치므로, 제한 수를 쓰는 단위는 이 기준으로 다시 나눈다.
function sameExecutionIdentity(a: ConcurrencyAccount, b: ConcurrencyAccount): boolean {
  if (a.id === b.id) return true;
  if (a.provider !== b.provider) return false;
  if (a.identityKey && a.identityKey === b.identityKey) return true;
  return (a.ompCredentialPins ?? []).some(pin => pin.hash !== "" && (b.ompCredentialPins ?? []).some(other => other.provider === pin.provider && other.hash === pin.hash));
}

/// 설정 행 하나가 곧 실제 실행 identity 하나다: 같은 identity의 Claude native 연결만 한 행으로 묶는다.
/// 같은 조직의 다른 사용자, 같은 사용자의 다른 워크스페이스는 서로 다른 행이다.
export function concurrencyRows<T extends ConcurrencyAccount>(members: readonly T[]): T[][] {
  const rows: T[][] = [];
  for (const target of concurrencyTargets(members)) {
    const matching = rows.filter(row => row.some(item => sameExecutionIdentity(item, target)));
    if (matching.length === 0) { rows.push([target]); continue; }
    // pin으로 두 행이 이어지면 하나로 합친다(전이적 같은 identity).
    const [first, ...rest] = matching;
    first.push(target);
    for (const row of rest) { first.push(...row); rows.splice(rows.indexOf(row), 1); }
  }
  return rows;
}

/// 화면에 보일 값. 연결마다 값이 다르면 `mixed`로 두고, 사용자가 고르기 전까지 어느 연결도 바꾸지 않는다.
export function concurrencyValue(targets: readonly { maxConcurrency: number }[]): number | "mixed" | null {
  if (!targets.length) return null;
  const first = targets[0].maxConcurrency;
  return targets.every(target => target.maxConcurrency === first) ? first : "mixed";
}

/// 값을 바꿔야 하는 연결의 id. 같은 값이면 서비스에 쓰지 않는다.
export function concurrencyUpdateIds(targets: readonly ConcurrencyAccount[], value: number): string[] {
  return targets.filter(target => target.maxConcurrency !== value).map(target => target.id);
}
