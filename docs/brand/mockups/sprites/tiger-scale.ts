/// 호랑이 저울: 남은 한도(쌀알)와 리셋까지 남은 시간(모래시계)을 견준다.
/// 고르게 쓰면 리셋 때 딱 바닥난다고 보고, 그보다 많이 남았으면 쌀 쪽이, 빨리 쓰고 있으면 시간 쪽이 무겁다.

/// 한도 라벨에서 주기 길이를 짐작한다. 버킷에 주기 필드가 없어서다. 모르면 null(기울이지 않음).
export function windowMs(label: string): number | null {
  const text = label.toLowerCase();
  if (/주간|weekly|week|mingguan/.test(text)) return 7 * 86_400_000;
  if (/5\s*시간|5\s*-?\s*h(ou)?r?|five/.test(text)) return 5 * 3_600_000;
  if (/일간|daily|day|harian/.test(text)) return 86_400_000;
  return null;
}

/// 기울기 -2..2. 양수 = 쌀 쪽이 무거움(많이 남음, 먼저 쓰기), 음수 = 시간 쪽이 무거움(빨리 쓰는 중).
/// 근거가 없으면(관측·리셋 시각·주기 모름) 0으로 두어 상태를 단정하지 않는다.
export function scaleTilt(remaining: number | null, resetsAt: number | null, label: string, now = Date.now()): number {
  const window = windowMs(label);
  if (remaining === null || resetsAt === null || window === null || resetsAt <= now) return 0;
  const timeLeft = Math.min(1, (resetsAt - now) / window);
  const diff = remaining / 100 - timeLeft;
  if (Math.abs(diff) < 0.1) return 0;
  return Math.sign(diff) * (Math.abs(diff) >= 0.3 ? 2 : 1);
}
