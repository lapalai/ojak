/// 스냅샷 읽기 순서 규칙. 읽기는 한 번에 하나다.
/// - 3초 주기 읽기(`load()`)는 진행 중인 읽기가 있으면 그 결과를 같이 쓴다.
/// - 변경(계정 삭제·수정·추가) 뒤의 다시 읽기(`load(false, true)`)는 변경 전에 시작된 읽기를 재사용하지 않는다.
///   그 읽기가 끝나기를 기다린 뒤 새로 읽는다. 재사용하면 변경 전 목록이 성공 알림과 함께 화면에 남는다.
export interface SnapshotLoader {
  load(refresh?: boolean, fresh?: boolean): Promise<void>;
  refreshing(): boolean;
}

export function snapshotLoader(read: (refresh: boolean) => Promise<void>, alive: () => boolean): SnapshotLoader {
  let flight: Promise<void> | null = null;
  let refreshing = false;
  const load = async (refresh = false, fresh = false): Promise<void> => {
    if (refresh && refreshing) return;
    if (!refresh && !fresh && flight) return flight;
    if (refresh) refreshing = true;
    try {
      while (flight) await flight;
      if (!alive()) return;
      const request = read(refresh);
      flight = request;
      try { await request; } finally { if (flight === request) flight = null; }
    } finally {
      if (refresh) refreshing = false;
    }
  };
  return { load, refreshing: () => refreshing };
}
