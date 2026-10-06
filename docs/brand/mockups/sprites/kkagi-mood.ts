import { remainingTone } from "./limits.ts";

/// 팝오버 깍이의 상태. 숫자는 카드에 그대로 있고, 깍이는 지금 상황을 한눈에 알리는 보조 표시다.
export type KkagiMood = "rest" | "carry" | "low" | "out" | "hurry";

/// 지금 쓰는 계정의 남은 % (쓰는 계정이 없으면 빈 배열), 안전 여유량, 곧 리셋 계정 여부로 상태를 정한다.
/// 우선순위: 쓰는 계정 소진 > 쓰는 계정 부족 > 곧 리셋 > 나르는 중 > 쉼.
export function kkagiMood(inUseRemaining: (number | null)[], reserve: number, expiring: boolean): KkagiMood {
  const known = inUseRemaining.filter((value): value is number => value !== null);
  if (known.length) {
    const tone = remainingTone(Math.min(...known), reserve);
    if (tone === "bad") return "out";
    if (tone === "warn") return "low";
  }
  if (expiring) return "hurry";
  return inUseRemaining.length ? "carry" : "rest";
}

/// 팝오버의 깍이 표시 설정. 팝오버와 대시보드가 같은 origin이라 localStorage로 공유한다.
export const KKAGI_STORAGE_KEY = "ojak.kkagi";
export function kkagiEnabled(): boolean {
  try { return localStorage.getItem(KKAGI_STORAGE_KEY) !== "off"; } catch { return true; }
}
