import type { Account, ObservedSession, ProcessIdentity, Session } from "./types.ts";

export interface AccountWork {
  id: string;
  tool: string;
  cwd: string | null;
  model: string | null;
  modelSource: "request" | "observation";
}

function sameProcess(a: ProcessIdentity | null, b: ProcessIdentity): boolean {
  return a !== null && a.pid === b.pid && a.startedAt === b.startedAt && a.bootId === b.bootId;
}

/// 실행·계정 신원이 확인된 작업만 센다. 최근 요청이나 sticky bridge 기록은 실행 중으로 추측하지 않는다.
/// 실행 중 관측한 계정이 시작 시 배정보다 우선하며, 같은 프로세스의 관리·관측 기록은 한 번만 센다.
export function activeAccountWork(members: readonly Account[], sessions: readonly Session[], observed: readonly ObservedSession[]): AccountWork[] {
  const ids = new Set(members.map(member => member.id));
  const confirmed = observed.filter(session => session.accountId && (session.verification === "runtime-confirmed" || session.verification === "upstream-confirmed"));
  const corresponds = (managed: Session, seen: ObservedSession) => sameProcess(managed.process, seen.process)
    || (!managed.process && managed.tool === seen.tool && Boolean(managed.nativeSessionId && managed.nativeSessionId === seen.nativeSessionId));
  const work: AccountWork[] = [];
  const seen = new Set<string>();
  for (const session of confirmed) {
    if (!ids.has(session.accountId!)) continue;
    const key = JSON.stringify(session.process);
    if (seen.has(key)) continue;
    seen.add(key);
    work.push({ id: session.id, tool: session.tool, cwd: session.cwd, model: session.model, modelSource: "observation" });
  }
  for (const session of sessions) {
    if (session.state !== "ACTIVE" || !ids.has(session.accountId) || confirmed.some(item => corresponds(session, item))) continue;
    work.push({ id: session.id, tool: session.tool, cwd: session.cwd, model: session.model, modelSource: "request" });
  }
  return work;
}
