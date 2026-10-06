/// 호랑이 저울 아이콘. 왼쪽 접시 = 쌀알(남은 한도), 오른쪽 접시 = 모래시계(리셋까지 시간).
/// 기울기는 `scaleTilt`가 정하고(+ 쌀 쪽이 내려감), 모래시계 색은 기존 잔여량 규칙(`remainingTone`)을 따른다.
/// 18×14 도트를 코드로 그려 이미지 파일 없이 어떤 배율에서도 선명하다.
type Cell = [number, number, "B" | "D" | "G" | "H" | "A"];

function cells(tilt: number, empty: boolean): Cell[] {
  const drop = empty ? -2 : tilt;
  const out: Cell[] = [];
  for (let y = 3; y <= 11; y++) out.push([8, y, "B"], [9, y, "B"]);
  for (let x = 5; x <= 12; x++) out.push([x, 12, "B"]);
  out.push([8, 2, "D"], [9, 2, "D"]);
  // 들보는 중심에서 계단식 대각선. 양 끝 줄에 접시가 매달린다.
  const beamY = (x: number) => 3 + Math.round((drop * (8.5 - x) / 7.5) * 1.5);
  for (let x = 1; x <= 16; x++) out.push([x, beamY(x), "D"]);
  const ly = beamY(2), ry = beamY(15);
  for (let y = ly + 1; y <= ly + 3; y++) out.push([2, y, "D"]);
  for (let y = ry + 1; y <= ry + 3; y++) out.push([15, y, "D"]);
  for (let x = 0; x <= 4; x++) out.push([x, ly + 4, "B"]);
  for (let x = 13; x <= 17; x++) out.push([x, ry + 4, "B"]);
  if (!empty) {
    out.push([1, ly + 3, "G"], [3, ly + 3, "G"]);
    if (tilt > 0) out.push([1, ly + 2, "G"], [2, ly + 2, "G"], [3, ly + 2, "G"]);
  }
  out.push([14, ry + 1, "H"], [16, ry + 1, "H"], [15, ry + 2, "A"], [14, ry + 3, "H"], [16, ry + 3, "H"], [15, ry + 3, "A"]);
  return out;
}

const FILL = { B: "var(--scale-brass)", D: "var(--scale-brass-dark)", G: "var(--kkagi-grain)", H: "currentColor", A: "var(--scale-sand)" };

export function TigerScale({ tilt, tone, empty, size = 28 }: { tilt: number; tone: string; empty: boolean; size?: number }) {
  return <svg className={`tiger-scale ${tone}`} width={size} height={(size * 14) / 18} viewBox="0 0 18 14" shapeRendering="crispEdges" aria-hidden="true">
    {cells(tilt, empty).map(([x, y, kind]) => <rect key={`${x}-${y}`} x={x} y={y} width={1} height={1} fill={FILL[kind]} />)}
  </svg>;
}
