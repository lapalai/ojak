import type { QuotaBucket } from "./types";

/// 버킷 상태. 관측이 오래됐거나 리셋 시각이 지났으면 소진 표시보다 먼저 `stale`로 본다.
/// 이미 리셋된 소진 버킷을 계속 0%로 보이지 않게 하려는 것이다(메뉴바의 Rust 계산과 같은 규칙).
export function bucketState(bucket: QuotaBucket, staleAfter: number, now = Date.now()): string {
  if (bucket.status === "stale") return "stale";
  if (!["known", "exhausted"].includes(bucket.status) || !bucket.observedAt) return "unknown";
  if (now - bucket.observedAt > staleAfter * 1000 || (bucket.resetsAt !== null && bucket.resetsAt <= now)) return "stale";
  if (bucket.status === "exhausted") return "exhausted";
  if (bucket.usedPercent === null || !Number.isFinite(bucket.usedPercent)) return "unknown";
  return bucket.usedPercent >= 100 ? "exhausted" : "known";
}

/// 팝오버에 그리는 한도 한 줄. `remaining`은 남은 %(0–100), 관측이 없으면 null.
export interface Limit { key: string; label: string; remaining: number | null; resetsAt: number | null; relevant: boolean }

/// 버킷 상태(bucketState)를 받아 남은 %로 바꾼다. 소진은 0, 신선한 관측이 없으면 null.
export function remainingOf(bucket: QuotaBucket, state: string): number | null {
  if (state === "exhausted") return 0;
  if (state !== "known" || bucket.usedPercent === null) return null;
  return Math.max(0, Math.min(100, 100 - bucket.usedPercent));
}

/// 이 값 이하로 남으면 주의 색으로 바꾼다. 공급자와 관계없이 모든 한도에 같은 기준을 쓴다.
export const WARN_REMAINING_PERCENT = 30;

/// 남은 % → 색 단계. 공급자 색은 쓰지 않는다(Codex 초록·Claude 주황이 상태처럼 읽히기 때문).
/// 넉넉함 = ok, 30% 이하 = warn, 안전 여유분 이하·소진 = bad, 관측 없음 = unknown.
export function remainingTone(remaining: number | null, reserve: number): "ok" | "warn" | "bad" | "unknown" {
  if (remaining === null) return "unknown";
  if (remaining <= reserve) return "bad";
  return remaining <= WARN_REMAINING_PERCENT ? "warn" : "ok";
}

/// 계정 버킷을 표시용 한도 줄로 만든다. 라벨(`Claude 5 Hour · 5시간`)은 마지막 부분만 쓰되,
/// 끝이 같은데 서로 다른 한도(Antigravity의 `Gemini · 주간`과 `Claude & GPT · 주간`)는 앞부분을 붙여 구분한다.
/// 같은 한도를 두 도구가 본 경우(끝·모델·값이 같음)만 하나로 합치고, 그때는 더 적게 남은 값을 쓴다.
/// 모델 전용 한도는 지금 그 모델을 쓸 때만 `relevant`다. 쓰는 모델이 없으면 모두 관련으로 본다.
export function limitsOf(buckets: QuotaBucket[], models: string[], stateOf: (bucket: QuotaBucket) => string): Limit[] {
  const parts = buckets.map(bucket => bucket.label.split(" · "));
  const shorts = parts.map(segments => segments[segments.length - 1]);
  const remaining = buckets.map(bucket => remainingOf(bucket, stateOf(bucket)));
  const limits = new Map<string, Limit>();
  buckets.forEach((bucket, index) => {
    const distinct = buckets.some((other, otherIndex) => otherIndex !== index && shorts[otherIndex] === shorts[index]
      && (other.model !== bucket.model || Math.abs((remaining[otherIndex] ?? -1) - (remaining[index] ?? -1)) >= 0.5));
    const label = distinct && parts[index].length > 1 ? `${parts[index][0]} ${shorts[index]}` : shorts[index];
    const limited = bucket.model?.toLowerCase();
    const relevant = !limited || models.length === 0 || models.some(model => model.toLowerCase().includes(limited));
    const limit = { key: bucket.id, label, remaining: remaining[index], resetsAt: bucket.resetsAt, relevant };
    const key = `${label}\u0000${bucket.model ?? ""}`;
    const existing = limits.get(key);
    if (!existing || (limit.remaining ?? 101) < (existing.remaining ?? 101)) limits.set(key, limit);
  });
  return [...limits.values()];
}

/// 카드 대표값: 지금 쓰는 모델과 관련된 한도 중 가장 적게 남은 값.
export function tightestOf(limits: Limit[]): number | null {
  let tightest: number | null = null;
  for (const limit of limits) {
    if (limit.relevant && limit.remaining !== null) tightest = tightest === null ? limit.remaining : Math.min(tightest, limit.remaining);
  }
  return tightest;
}

/// 공급자 전체 소진: 배정에 포함된 계정이 하나 이상 있고, 그 계정이 모두 `resting`(소진·차단)일 때만.
/// 반환값은 가장 먼저 풀리는 시각(모르면 null). 하나라도 쓸 수 있거나 판정이 불확실하면 `false`로 띄우지 않는다.
export function allResting(verdicts: { kind: string; until: number | null; off: boolean }[]): { until: number | null } | false {
  const included = verdicts.filter(item => !item.off);
  if (!included.length || included.some(item => item.kind !== "resting")) return false;
  const times = included.map(item => item.until).filter((value): value is number => value !== null && Number.isFinite(value));
  return { until: times.length ? Math.min(...times) : null };
}

