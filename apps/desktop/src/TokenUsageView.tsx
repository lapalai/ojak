import { useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { Info, RefreshCw } from "lucide-react";
import { Busy, ErrorMessage } from "./components";
import { localUsage, toApiError } from "./api";
import { KNOWN_WARNINGS, USAGE_FILTERS, boundedStart, costKind, formatCount, formatPercent, formatUsd, nextPoll, orderedModels, orderedTools, readingProgress, tokenParts, usageRange, usageState } from "./local-usage";
import type { LocalUsageReport, UsageFilter, UsageRange, UsageTotals } from "./local-usage";
import { absoluteTime, relativeTime, toolNames, useCharacters, useWindowVisible } from "./state";
import type { ApiError } from "./types";
import { intlLocale, t } from "./i18n";
import tigerPlay from "./assets/tiger-play.png";
import tigerStrip from "./assets/yakgwa-tiger.png";
import magpieStrip from "./assets/yakgwa-magpie.png";
import yakgwaStrip from "./assets/yakgwa-yakgwa.png";

/// 모델 표는 처음에 이만큼만 보이고 나머지는 펼친다.
const MODEL_PREVIEW = 8;
const toolColors: Record<string, string> = { omp: "var(--p-grok)", claude: "var(--p-claude)", codex: "var(--p-codex)" };
const partColors = { input: "var(--p-gemini)", output: "var(--p-claude)", cacheRead: "var(--p-codex)", cacheWrite: "var(--p-zai)" } as const;

interface Loaded { filter: UsageFilter; range: UsageRange; report: LocalUsageReport | null; error: ApiError | null }

/// 기간 조회와 읽는 중 갱신. 읽기가 끝나지 않았을 때만 다시 조회하고, 필터가 바뀌거나 화면이 사라지면 대기 중인 조회와 응답을 모두 버린다.
/// 필터마다 이펙트가 새로 생겨 `cancelled`가 따로이므로, 늦게 도착한 이전 필터의 응답은 화면에 닿지 못한다.
function useLocalUsage(filter: UsageFilter, reloadKey: number, visible: boolean): Loaded | null {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  // 숨겨진 창은 진행 조회를 하지 않는다. 읽기 자체는 네이티브 작업이 창과 무관하게 이어 가고, 다시 보이면 최신 값을 읽는다.
  useEffect(() => {
    if (!visible) return;
    let cancelled = false;
    let timer: number | undefined;
    let previous: LocalUsageReport | null = null;
    let stalled = 0;
    setLoaded(current => current?.filter === filter ? current : null);
    const run = async () => {
      const range = usageRange(filter, Date.now());
      try {
        const report = await localUsage(range);
        if (cancelled) return;
        setLoaded({ filter, range, report, error: null });
        const next = nextPoll(previous, report, stalled);
        previous = report;
        if (next) { stalled = next.stalled; timer = window.setTimeout(() => { void run(); }, next.delayMs); }
      } catch (failure) {
        if (cancelled) return;
        // 읽던 값이 있으면 그대로 두고 오류를 함께 보인다. 자동 재시도는 하지 않는다(다시 시도 버튼).
        setLoaded(current => ({ filter, range, report: current?.filter === filter ? current.report : null, error: toApiError(failure) }));
      }
    };
    void run();
    return () => { cancelled = true; if (timer !== undefined) window.clearTimeout(timer); };
  }, [filter, reloadKey, visible]);
  return loaded?.filter === filter ? loaded : null;
}

const rangeFormat = new Intl.DateTimeFormat(intlLocale, { year: "numeric", month: "short", day: "numeric" });
const dateFormat = rangeFormat;

/// 기간과 실제 날짜 범위. 오늘은 하루, 이번 달은 1일~오늘, 전체는 가장 오래된 기록~오늘이다.
/// 전체 범위의 시작은 지금까지 읽은 기록 기준이므로, 처음 읽는 중에는 더 앞으로 늘어날 수 있다.
function periodLabel(filter: UsageFilter, range: UsageRange, oldest: number | null): string {
  const today = range.untilMs - 1;
  if (filter === "today") return t("tokens.period.today", { date: rangeFormat.format(today) });
  const from = filter === "month" ? range.sinceMs : oldest;
  if (from === null) return t("tokens.period.all");
  return t(filter === "month" ? "tokens.period.month" : "tokens.period.allRange", { range: rangeFormat.formatRange(Math.min(from, today), today) });
}

const count = (value: number) => formatCount(value, intlLocale);

/// 환산액 칸. 가격을 모르는 모델이 섞이면 "일부만"이라고 같은 줄에 밝히고, 하나도 모르면 0이 아니라 환산 불가로 적는다.
function CostCell({ totals }: { totals: UsageTotals }) {
  const kind = costKind(totals);
  if (kind === "none") return <span className="muted">—</span>;
  if (kind === "unpriced") return <span className="tag warning" title={t("tokens.cost.unpricedNote")}>{t("tokens.cost.unpricedTag")}</span>;
  return <>{formatUsd(totals.costUsd ?? 0, intlLocale)}{kind === "partial" && <> <span className="tag warning" title={t("tokens.cost.partial", { tokens: count(totals.unpricedTokens) })}>{t("tokens.cost.partialTag")}</span></>}</>;
}

/// 4칸짜리 동작 그림(가로 띠) 한 장을 칸 단위로 바꿔 보여 준다. 칸 전환은 CSS `steps`라 사이 그림이 번지지 않는다.
/// `w`·`h`는 한 칸의 화면 크기, 그림은 2배 해상도라 픽셀이 선명하다.
function Sprite({ src, w, h, className }: { src: string; w: number; h: number; className: string }) {
  return <i className={`token-sprite ${className}`} style={{ width: w, height: h, backgroundImage: `url(${src})`, backgroundSize: `${w * 4}px ${h}px` } as CSSProperties} />;
}

/// 읽는 동안: 호랑이가 걸어와 약과 냄새를 맡고, 집어 먹고, 오물거린 뒤 다시 걷는다. 약과는 한 입씩 줄어 부스러기가 됐다가 다시 놓인다.
/// 깍이는 옆에서 콩콩 뛰다 약과를 물고 선다. 호랑이 크기는 읽은 비율만 따른다(사용량과 무관).
/// 모두 CSS 애니메이션이며, 읽기가 끝나면 이 장면이 사라져 멈춘다. 움직임 줄이기 설정에서는 첫 동작에서 멈춘 그림이다.
function ReadingScene({ progress }: { progress: number }) {
  const scale = 0.6 + 0.4 * progress;
  return <div className="token-play" aria-hidden="true" style={{ "--grow": scale } as CSSProperties}>
    <div className="token-tiger-track"><Sprite src={tigerStrip} w={106.5} h={96} className="token-tiger-sprite" /></div>
    <Sprite src={yakgwaStrip} w={22.5} h={20} className="token-yakgwa-sprite" />
    <Sprite src={magpieStrip} w={80.5} h={56} className="token-magpie-sprite" />
  </div>;
}

function Scene({ characters, reading, progress }: { characters: boolean; reading: boolean; progress: number }) {
  return <div className={`token-landscape${characters ? "" : " plain"}`}>
    <i className="token-ridge far" aria-hidden="true" />
    <i className="token-ridge mid" aria-hidden="true" />
    <i className="token-ridge near" aria-hidden="true" />
    <i className="token-cloud" aria-hidden="true" />
    <div className="token-caption">
      <strong>{characters && reading ? t("tokens.scene.reading") : t("tokens.scene.title")}</strong>
      <small>{characters && reading ? t("tokens.scene.readingNote") : t("tokens.scene.note")}</small>
    </div>
    {characters && (reading
      ? <ReadingScene progress={progress} />
      : <img className="token-art" src={tigerPlay} alt="" width={705} height={288} draggable={false} />)}
  </div>;
}

export function TokenUsageView() {
  const characters = useCharacters();
  const visible = useWindowVisible();
  const [filter, setFilter] = useState<UsageFilter>("today");
  const [reloadKey, setReloadKey] = useState(0);
  const [showAllModels, setShowAllModels] = useState(false);
  const loaded = useLocalUsage(filter, reloadKey, visible);
  const report = loaded?.report ?? null;
  const state = report ? usageState(report) : null;
  const partial = Boolean(report && (report.scanning || report.truncated));
  const bounded = report && loaded ? boundedStart(report, loaded.range) : null;
  const models = useMemo(() => report ? orderedModels(report.models) : [], [report]);
  const shownModels = showAllModels ? models : models.slice(0, MODEL_PREVIEW);
  const overlap = Boolean(report?.warnings.includes("CROSS_TOOL_OVERLAP"));
  const busy = !loaded || Boolean(report?.scanning);
  const unknownWarnings = report ? report.warnings.filter(code => !(KNOWN_WARNINGS as readonly string[]).includes(code)) : [];

  return <section className="view token-view" aria-labelledby="tokens-title">
    <div className="top">
      <div><h1 id="tokens-title">{t("nav.tokens")}</h1><div className="sub">{t("tokens.subtitle")}</div></div>
      <div className="button-row">
        <div className="segmented" role="group" aria-label={t("tokens.filter.aria")}>{USAGE_FILTERS.map(value => <button type="button" key={value} aria-pressed={filter === value} onClick={() => { setFilter(value); setShowAllModels(false); }}>{t(`tokens.filter.${value}`)}</button>)}</div>
        <button type="button" className="icon-button" onClick={() => setReloadKey(key => key + 1)} disabled={busy && !loaded?.error} title={t("tokens.refresh")} aria-label={t("tokens.refresh")}><RefreshCw size={14} className={busy && !loaded?.error ? "spinner" : undefined} /></button>
      </div>
    </div>

    <div className="token-scroll">
      <Scene characters={characters && visible} reading={Boolean(report?.scanning) || !loaded} progress={report ? readingProgress(report) : 0} />
      <div className="token-ledger">
        {!loaded && <div className="token-reading" role="status"><Busy label={t("tokens.loading")} /></div>}
        {loaded?.error && <><ErrorMessage error={loaded.error} />{report && <p className="field-help">{t("tokens.error.stale")}</p>}<div className="row-actions inline"><button type="button" onClick={() => setReloadKey(key => key + 1)}>{t("tokens.error.retry")}</button></div></>}
        {report && state === "data" && loaded && <>
          <div className="token-figures">
            <div className="token-main">
              <p className="token-label">{t("tokens.metric.label")} <span className="muted">· {periodLabel(filter, loaded.range, report.coverageStart)}</span></p>
              <div className="token-number" aria-label={t("tokens.metric.aria", { count: count(report.totals.totalTokens) })}>{count(report.totals.totalTokens)}<small>{t("tokens.metric.unit")}</small></div>
              <div className="tools">
                {partial && <span className="tag warning">{t("tokens.metric.partial")}</span>}
                {overlap && <span className="tag">{t("tokens.metric.overlap")}</span>}
              </div>
            </div>
            <div className="token-cost">
              <p className="token-label">{t("tokens.cost.label")}</p>
              {costKind(report.totals) === "unpriced"
                ? <div className="token-money unpriced">{t("tokens.cost.unpricedTag")}</div>
                : <div className="token-money">{formatUsd(report.totals.costUsd ?? 0, intlLocale)}{costKind(report.totals) === "partial" && <small>{t("tokens.cost.partialTag")}</small>}</div>}
              <p className="token-note">{t("tokens.cost.note")}</p>
              {costKind(report.totals) === "unpriced" && <p className="token-note">{t("tokens.cost.unpricedNote")}</p>}
              {costKind(report.totals) === "partial" && <p className="token-note">{t("tokens.cost.partial", { tokens: count(report.totals.unpricedTokens) })}</p>}
            </div>
          </div>
          <div className="token-parts">
            <div className="token-bar" role="img" aria-label={t("tokens.parts.aria")}>{tokenParts(report.totals).filter(part => part.value > 0).map(part => <i key={part.key} style={{ flexGrow: part.value, "--c": partColors[part.key] } as CSSProperties} />)}</div>
            <ul className="token-legend">{tokenParts(report.totals).map(part => <li key={part.key}><span className="dot" style={{ "--c": partColors[part.key] } as CSSProperties} aria-hidden="true" /><span>{t(`tokens.part.${part.key}`)}</span><b>{count(part.value)}</b><small>{formatPercent(part.share)}%</small></li>)}</ul>
          </div>
        </>}
        {report && state !== "data" && state !== "reading" && state && <div className="token-empty" role="status"><strong>{t(`tokens.empty.${state}.title`)}</strong><p>{t(`tokens.empty.${state}.body`)}</p></div>}
        {report && <p className="token-status" role="status" aria-live="polite">
          {report.scanning ? <Busy label={t("tokens.status.reading", { percent: formatPercent(readingProgress(report)), pending: count(report.filesPending) })} />
            : report.truncated ? t("tokens.status.paused", { pending: count(report.filesPending) })
            : report.indexedAt ? t("tokens.status.indexed", { time: relativeTime(report.indexedAt) }) : null}
          {bounded !== null && <> {t("tokens.status.bounded", { date: dateFormat.format(bounded) })}</>}
        </p>}
      </div>
    </div>

    {report && (report.warnings.length > 0) && <div className="panel token-notes">
      <header><h2>{t("tokens.notes.title")}</h2></header>
      <ul>{[...KNOWN_WARNINGS.filter(code => report.warnings.includes(code)), ...unknownWarnings].map(code => <li key={code}><Info size={14} aria-hidden="true" /><span>{(KNOWN_WARNINGS as readonly string[]).includes(code) ? t(`tokens.warning.${code as typeof KNOWN_WARNINGS[number]}`) : t("tokens.warning.unknown", { code })}</span></li>)}</ul>
    </div>}

    {report && state === "data" && <>
      <div className="panel">
        <header><h2>{t("tokens.tools.title")}</h2></header>
        <div className="route-table-scroll"><table>
          <thead><tr><th>{t("tokens.col.tool")}</th><th className="num">{t("tokens.col.tokens")}</th><th className="num">{t("tokens.col.cost")}</th><th className="num">{t("tokens.col.sessions")}</th></tr></thead>
          <tbody>{orderedTools(report.tools).map(tool => <tr key={tool.tool}>
            <td><span className="dot lead" style={{ "--c": toolColors[tool.tool] } as CSSProperties} aria-hidden="true" />{toolNames[tool.tool]}</td>
            <td className="num">{count(tool.totalTokens)}</td>
            <td className="num"><CostCell totals={tool} /></td>
            <td className="num">{count(tool.sessions)}</td>
          </tr>)}</tbody>
        </table></div>
      </div>

      <div className="panel">
        <header><h2>{t("tokens.models.title")}</h2><span className="aside">{t("tokens.models.count", { count: models.length })}</span></header>
        <div className="route-table-scroll"><table>
          <thead><tr><th>{t("tokens.col.model")}</th><th>{t("tokens.col.tool")}</th><th className="num">{t("tokens.col.tokens")}</th><th className="num">{t("tokens.col.cost")}</th></tr></thead>
          <tbody>{shownModels.map(model => <tr key={`${model.tool}\u0000${model.model}`}>
            <td className="path-value">{model.model}</td>
            <td>{toolNames[model.tool] ?? model.tool}</td>
            <td className="num">{count(model.totalTokens)}</td>
            <td className="num"><CostCell totals={model} /></td>
          </tr>)}</tbody>
        </table></div>
        {models.length > MODEL_PREVIEW && <div className="row-actions"><button type="button" className="text-button" onClick={() => setShowAllModels(value => !value)}>{showAllModels ? t("tokens.models.less") : t("tokens.models.more", { count: models.length })}</button></div>}
      </div>
    </>}

    {report && <details className="panel token-method">
      <summary>{t("tokens.method.title")}</summary>
      <ul>
        <li>{t("tokens.method.local")}</li>
        <li>{t("tokens.method.sum")}</li>
        <li>{t("tokens.method.bridge")}</li>
        <li>{t("tokens.method.cost")}</li>
        <li>{t("tokens.method.dedupe", { duplicates: count(report.duplicateRecords), excluded: count(report.excludedRecords) })}</li>
        {report.coverageStart !== null && report.coverageEnd !== null && <li>{t("tokens.method.coverage", { from: absoluteTime(report.coverageStart), to: absoluteTime(report.coverageEnd) })}</li>}
        <li>{t("tokens.method.privacy")}</li>
      </ul>
    </details>}
  </section>;
}
