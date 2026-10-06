import type { KkagiMood } from "./kkagi-mood.ts";

// 15×8 옆모습 도트 깍이. 문자 하나가 한 칸: K 검정 깃, W 흰 배·날개 무늬, T 청록 꼬리, E 눈, B 부리, . 빈칸.
// 로고(오작교 까치)의 흑백 깃·흰 배·긴 청록 꼬리를 따른다. 이미지 파일 없이 코드로 그려 어떤 배율에서도 선명하다.
const BODY = [
  "..........KKK..",
  ".........KKEKB.",
  "TT......KKKKK..",
  ".TTT..KKKKKKK..",
  "...TTKKWWKWWK..",
  ".....KKKWWWWK..",
  "........KWWK...",
  ".........K.K...",
];
const COLORS: Record<string, string> = { K: "var(--kkagi-ink)", W: "var(--kkagi-belly)", T: "var(--kkagi-tail)", E: "var(--kkagi-eye)", B: "var(--kkagi-ink)" };
const CELLS = BODY.flatMap((row, y) => [...row].map((cell, x) => ({ cell, x, y }))).filter(({ cell }) => cell !== ".");

/// 상태별 소품: 부리 끝 쌀알(나르는 중), 쌀알 세 개(곧 리셋), 땀방울(부족). 소진은 눈이 빨간 X.
function extras(mood: KkagiMood): { x: number; y: number; fill: string }[] {
  if (mood === "carry") return [{ x: 14, y: 1, fill: "var(--kkagi-grain)" }];
  if (mood === "hurry") return [{ x: 14, y: 1, fill: "var(--kkagi-grain)" }, { x: 14, y: 0, fill: "var(--kkagi-grain)" }, { x: 13, y: 2, fill: "var(--kkagi-grain)" }];
  if (mood === "low") return [{ x: 8, y: 0, fill: "var(--accent)" }, { x: 7, y: 1, fill: "var(--accent)" }];
  return [];
}

export function Kkagi({ mood, size = 34 }: { mood: KkagiMood; size?: number }) {
  return <svg className={`kkagi kkagi-${mood}`} width={size} height={(size * 8) / 15} viewBox="0 0 15 8" shapeRendering="crispEdges" aria-hidden="true">
    <g className="kkagi-body">
      {CELLS.map(({ cell, x, y }) => <rect key={`${x}-${y}`} x={x} y={y} width={1} height={1} fill={mood === "out" && cell === "E" ? "var(--danger)" : COLORS[cell]} />)}
    </g>
    {extras(mood).map(({ x, y, fill }) => <rect key={`e${x}-${y}`} className="kkagi-extra" x={x} y={y} width={1} height={1} fill={fill} />)}
  </svg>;
}
