/// PC에 남은 omp·Claude CLI·Codex 사용 기록의 토큰·API 환산 비용(네이티브 `local_usage`).
/// 화면·네이티브 호출과 무관한 순수 모듈이다(타입, 응답 검증, 기간 계산, 조회 정책, 숫자 표기). 호출은 api.ts의 `localUsage`.

export type UsageFilter = "today" | "month" | "all";
export const USAGE_FILTERS: UsageFilter[] = ["today", "month", "all"];

/// `sinceMs`는 포함, `untilMs`는 제외(Unix ms). `sinceMs`가 null이면 전체 기간이다.
export interface UsageRange { sinceMs: number | null; untilMs: number }

/// `inputTokens`는 캐시 입력을 뺀 값이고 `outputTokens`는 추론 토큰을 포함한다. 합계는 네이티브가 센 값을 그대로 쓴다.
/// `costUsd`는 가격을 아는 모델만 합친 API 요금 환산액이다. 아무것도 환산하지 못하면 null이며, 0은 무료가 아니라 환산액 0이다.
export interface UsageTotals {
  inputTokens: number; outputTokens: number; cacheReadTokens: number; cacheWriteTokens: number;
  totalTokens: number; costUsd: number | null; unpricedTokens: number;
}
export type UsageTool = "omp" | "claude" | "codex";
export interface ToolUsage extends UsageTotals { tool: UsageTool; sessions: number }
export interface ModelUsage extends UsageTotals { tool: string; model: string }
export interface LocalUsageReport {
  indexedAt: number | null; scanning: boolean; truncated: boolean;
  coverageStart: number | null; coverageEnd: number | null;
  filesScanned: number; filesPending: number; bytesRead: number; bytesTotal: number;
  totals: UsageTotals; tools: ToolUsage[]; models: ModelUsage[];
  /// 기계 코드. 화면 문구는 `tokens.warning.<코드>`에서 찾는다.
  warnings: string[]; excludedRecords: number; duplicateRecords: number;
}

/// 화면에서 쓰는 기간 경계는 이 컴퓨터의 시간대 기준이다.
export function usageRange(filter: UsageFilter, now: number): UsageRange {
  const date = new Date(now);
  const sinceMs = filter === "all" ? null
    : filter === "today" ? new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime()
    : new Date(date.getFullYear(), date.getMonth(), 1).getTime();
  return { sinceMs, untilMs: now + 1 };
}

/// 응답 값이 규칙에 어긋나면 던진다. `ApiError` 모양이라 화면이 `error.code.LOCAL_USAGE_INVALID` 문구로 보여 준다(어느 필드인지는 원문에만 남는다).
function fail(path: string): never {
  throw { code: "LOCAL_USAGE_INVALID", message: `invalid local usage report: ${path}`, retryable: true };
}

/// 개수는 안전한 음이 아닌 정수여야 한다. 부동소수·음수·정밀도를 잃은 값은 조용히 고치지 않고 응답 전체를 거부한다.
function count(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) fail(path);
  return value;
}
function time(value: unknown, path: string): number | null {
  if (value === null) return null;
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) fail(path);
  return value;
}
function flag(value: unknown, path: string): boolean {
  if (typeof value !== "boolean") fail(path);
  return value;
}
function name(value: unknown, path: string): string {
  if (typeof value !== "string" || value.length === 0) fail(path);
  return value;
}
function totals(value: UsageTotals, path: string): UsageTotals {
  const cost: unknown = value.costUsd;
  if (cost !== null && (typeof cost !== "number" || !Number.isFinite(cost) || cost < 0)) fail(`${path}.costUsd`);
  return {
    inputTokens: count(value.inputTokens, `${path}.inputTokens`),
    outputTokens: count(value.outputTokens, `${path}.outputTokens`),
    cacheReadTokens: count(value.cacheReadTokens, `${path}.cacheReadTokens`),
    cacheWriteTokens: count(value.cacheWriteTokens, `${path}.cacheWriteTokens`),
    totalTokens: count(value.totalTokens, `${path}.totalTokens`),
    costUsd: cost,
    unpricedTokens: count(value.unpricedTokens, `${path}.unpricedTokens`),
  };
}
const TOOLS: readonly string[] = ["omp", "claude", "codex"];

/// 네이티브 응답(serde가 모양을 보장한다)의 값만 검증해 화면 타입으로 만든다. 값이 규칙에 맞지 않으면 던진다. 잘못된 숫자를 보여 주는 것보다 오류가 낫다.
export function parseLocalUsageReport(raw: unknown): LocalUsageReport {
  if (typeof raw !== "object" || raw === null) fail("report");
  const report = raw as LocalUsageReport;
  if (!Array.isArray(report.tools)) fail("tools");
  if (!Array.isArray(report.models)) fail("models");
  if (!Array.isArray(report.warnings)) fail("warnings");
  if (typeof report.totals !== "object" || report.totals === null) fail("totals");
  return {
    indexedAt: time(report.indexedAt, "indexedAt"),
    scanning: flag(report.scanning, "scanning"),
    truncated: flag(report.truncated, "truncated"),
    coverageStart: time(report.coverageStart, "coverageStart"),
    coverageEnd: time(report.coverageEnd, "coverageEnd"),
    filesScanned: count(report.filesScanned, "filesScanned"),
    filesPending: count(report.filesPending, "filesPending"),
    bytesRead: count(report.bytesRead, "bytesRead"),
    bytesTotal: count(report.bytesTotal, "bytesTotal"),
    totals: totals(report.totals, "totals"),
    tools: report.tools.map((item, index) => {
      const path = `tools[${index}]`;
      if (typeof item !== "object" || item === null) fail(path);
      if (!TOOLS.includes(name(item.tool, `${path}.tool`))) fail(`${path}.tool`);
      return { ...totals(item, path), tool: item.tool, sessions: count(item.sessions, `${path}.sessions`) };
    }),
    models: report.models.map((item, index) => {
      const path = `models[${index}]`;
      if (typeof item !== "object" || item === null) fail(path);
      return { ...totals(item, path), tool: name(item.tool, `${path}.tool`), model: name(item.model, `${path}.model`) };
    }),
    warnings: [...new Set(report.warnings.filter((code): code is string => typeof code === "string" && /^[A-Z0-9_]{1,64}$/.test(code)))],
    excludedRecords: count(report.excludedRecords, "excludedRecords"),
    duplicateRecords: count(report.duplicateRecords, "duplicateRecords"),
  };
}

/// 읽기가 끝나지 않았을 때만 다시 조회한다. 진행(파일 수·합계 변화)이 없으면 간격을 늘린다. `null`이면 조회를 멈춘다.
export const POLL_BASE_MS = 1_500;
export const POLL_STEP_MS = 1_000;
export const POLL_MAX_MS = 8_000;
export function nextPoll(previous: LocalUsageReport | null, next: LocalUsageReport, stalled: number): { delayMs: number; stalled: number } | null {
  if (!next.scanning) return null;
  const progressed = !previous || previous.bytesRead !== next.bytesRead || previous.filesScanned !== next.filesScanned || previous.filesPending !== next.filesPending || previous.totals.totalTokens !== next.totals.totalTokens;
  const misses = progressed ? 0 : stalled + 1;
  return { delayMs: Math.min(POLL_MAX_MS, POLL_BASE_MS + misses * POLL_STEP_MS), stalled: misses };
}

/// 숫자 영역이 어떤 상태인지. 합계가 0이어도 "읽는 중"·"색인 열기 실패"·"기록 없음"·"이 기간에 없음"은 서로 다르게 말한다.
export type UsageState = "data" | "reading" | "unavailable" | "none" | "period-empty";
export function usageState(report: LocalUsageReport): UsageState {
  if (report.totals.totalTokens > 0 || report.tools.length > 0 || report.models.length > 0) return "data";
  if (report.warnings.includes("INDEX_UNAVAILABLE")) return "unavailable";
  if (report.scanning) return "reading";
  // 읽을 기록 폴더가 없거나 읽은 파일이 하나도 없으면 이 PC에서 기록 자체를 못 찾은 것이다.
  return report.warnings.includes("NO_SOURCES") || report.indexedAt === null || report.filesScanned === 0 ? "none" : "period-empty";
}

/// 환산액의 종류. `unpriced`는 가격을 아는 모델이 하나도 없다는 뜻이며 0원이 아니다.
export type CostKind = "none" | "unpriced" | "partial" | "priced";
export function costKind(value: UsageTotals): CostKind {
  if (value.costUsd === null) return value.unpricedTokens > 0 || value.totalTokens > 0 ? "unpriced" : "none";
  return value.unpricedTokens > 0 ? "partial" : "priced";
}

export type TokenPartKey = "input" | "output" | "cacheRead" | "cacheWrite";
/// 합계를 이루는 네 구성 요소와 그 비율(구성 요소 합계 대비). 합이 0이면 비율은 모두 0이다.
export function tokenParts(value: UsageTotals): { key: TokenPartKey; value: number; share: number }[] {
  const parts: { key: TokenPartKey; value: number }[] = [
    { key: "input", value: value.inputTokens }, { key: "output", value: value.outputTokens },
    { key: "cacheRead", value: value.cacheReadTokens }, { key: "cacheWrite", value: value.cacheWriteTokens },
  ];
  const sum = parts.reduce((acc, part) => acc + part.value, 0);
  return parts.map(part => ({ ...part, share: sum > 0 ? part.value / sum : 0 }));
}

const TOOL_ORDER: Record<string, number> = { omp: 0, claude: 1, codex: 2 };
/// 도구는 고정 순서, 모델은 도구 안에서 토큰이 많은 순(같으면 이름순)이다. 순위 번호는 붙이지 않는다.
export function orderedTools(tools: ToolUsage[]): ToolUsage[] {
  return [...tools].sort((a, b) => TOOL_ORDER[a.tool] - TOOL_ORDER[b.tool]);
}
export function orderedModels(models: ModelUsage[]): ModelUsage[] {
  return [...models].sort((a, b) => (TOOL_ORDER[a.tool] ?? 99) - (TOOL_ORDER[b.tool] ?? 99) || a.tool.localeCompare(b.tool) || b.totalTokens - a.totalTokens || a.model.localeCompare(b.model));
}

/// 아직 읽지 못한 파일이 남아 있을 때, 지금까지 읽은 기록이 시작되는 시각. 요청한 기간이 그보다 앞서 시작하면 그 앞 기록은 집계에 없을 수 있다.
export function boundedStart(report: LocalUsageReport, range: UsageRange): number | null {
  if (!report.truncated || report.coverageStart === null) return null;
  return range.sinceMs === null || report.coverageStart > range.sinceMs ? report.coverageStart : null;
}

/// 첫 읽기 진행률(0–1). 장면의 호랑이 크기에만 쓴다. 토큰 양과는 관계없다.
/// 파일 크기가 제각각이라 읽은 바이트로 센다. 전체 크기를 모르면(0) 시작 크기(0)로 둔다.
export function readingProgress(report: Pick<LocalUsageReport, "bytesRead" | "bytesTotal">): number {
  return report.bytesTotal > 0 ? Math.min(1, report.bytesRead / report.bytesTotal) : 0;
}

/// 화면 문구가 있는 경고 코드(`tokens.warning.<코드>`). 여기 없는 코드는 일반 문구로 보인다. 순서가 화면 순서다.
export const KNOWN_WARNINGS = [
  "INDEX_UNAVAILABLE", "NO_SOURCES", "INDEX_TRUNCATED", "SOURCE_UNREADABLE", "SOURCE_PARTIAL", "CLOCK_SKEW",
  "PRICING_UNKNOWN", "CROSS_TOOL_OVERLAP", "ATTRIBUTION_UNAVAILABLE",
] as const;

const formats = new Map<string, { count: Intl.NumberFormat; usd: Intl.NumberFormat }>();
function formatsFor(tag: string) {
  let found = formats.get(tag);
  if (!found) {
    found = { count: new Intl.NumberFormat(tag, { maximumFractionDigits: 0 }), usd: new Intl.NumberFormat(tag, { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 2 }) };
    formats.set(tag, found);
  }
  return found;
}
/// 토큰·개수는 줄이지 않고 자릿수 구분만 한다.
export function formatCount(value: number, tag: string): string {
  return formatsFor(tag).count.format(value);
}
/// 0보다 크지만 1센트 미만이면 0으로 보이지 않게 "<$0.01"로 적는다.
export function formatUsd(value: number, tag: string): string {
  const { usd } = formatsFor(tag);
  return value > 0 && value < 0.005 ? `<${usd.format(0.01)}` : usd.format(value);
}
/// 0%로 반올림되는 양수는 "<1"로 적는다.
export function formatPercent(share: number): string {
  const rounded = Math.round(share * 100);
  return share > 0 && rounded === 0 ? "<1" : String(rounded);
}
