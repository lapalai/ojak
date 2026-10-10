import { useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { Info, RefreshCw } from "lucide-react";
import { Busy, ErrorMessage } from "./components";
import { localUsage, toApiError } from "./api";
import { KNOWN_WARNINGS, USAGE_FILTERS, boundedStart, costKind, formatCount, formatPercent, formatUsd, nextPoll, orderedModels, orderedTools, tokenParts, usageRange, usageState } from "./local-usage";
import type { LocalUsageReport, UsageFilter, UsageRange, UsageTotals } from "./local-usage";
import { absoluteTime, relativeTime, toolNames, useCharacters } from "./state";
import type { ApiError } from "./types";
import { intlLocale, t } from "./i18n";
import tigerPlay from "./assets/tiger-play.png";

/// 모델 표는 처음에 이만큼만 보이고 나머지는 펼친다.
const MODEL_PREVIEW = 8;
const toolColors: Record<string, string> = { omp: "var(--p-grok)", claude: "var(--p-claude)", codex: "var(--p-codex)" };
const partColors = { input: "var(--p-gemini)", output: "var(--p-claude)", cacheRead: "var(--p-codex)", cacheWrite: "var(--p-zai)" } as const;

interface Loaded { filter: UsageFilter; range: UsageRange; report: LocalUsageReport | null; error: ApiError | null }

/// 기간 조회와 읽는 중 갱신. 읽기가 끝나지 않았을 때만 다시 조회하고, 필터가 바뀌거나 화면이 사라지면 대기 중인 조회와 응답을 모두 버린다.
/// 필터마다 이펙트가 새로 생겨 `cancelled`가 따로이므로, 늦게 도착한 이전 필터의 응답은 화면에 닿지 못한다.
function useLocalUsage(filter: UsageFilter, reloadKey: number): Loaded | null {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    let previous: LocalUsageReport | null = null;
    let stalled = 0;
    setLoaded(null);
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
  }, [filter, reloadKey]);
  return loaded?.filter === filter ? loaded : null;
}

const dayFormat = new Intl.DateTimeFormat(intlLocale, { month: "short", day: "numeric" });
const monthFormat = new Intl.DateTimeFormat(intlLocale, { year: "numeric", month: "long" });
const dateFormat = new Intl.DateTimeFormat(intlLocale, { year: "numeric", month: "short", day: "numeric" });

function periodLabel(filter: UsageFilter, range: UsageRange): string {
  return filter === "all" ? t("tokens.period.all")
    : filter === "today" ? t("tokens.period.today", { date: dayFormat.format(range.untilMs - 1) })
    : t("tokens.period.month", { month: monthFormat.format(range.untilMs - 1) });
}

const count = (value: number) => formatCount(value, intlLocale);

/// 환산액 칸. 가격을 모르는 모델이 섞이면 "일부만"이라고 같은 줄에 밝히고, 하나도 모르면 0이 아니라 환산 불가로 적는다.
function CostCell({ totals }: { totals: UsageTotals }) {
  const kind = costKind(totals);
  if (kind === "none") return <span className="muted">—</span>;
  if (kind === "unpriced") return <span className="tag warning" title={t("tokens.cost.unpricedNote")}>{t("tokens.cost.unpricedTag")}</span>;
  return <>{formatUsd(totals.costUsd ?? 0, intlLocale)}{kind === "partial" && <> <span className="tag warning" title={t("tokens.cost.partial", { tokens: count(totals.unpricedTokens) })}>{t("tokens.cost.partialTag")}</span></>}</>;
}

function Scene({ characters }: { characters: boolean }) {
  return <div className={`token-landscape${characters ? "" : " plain"}`}>
    <i className="token-ridge far" aria-hidden="true" />
    <i className="token-ridge mid" aria-hidden="true" />
    <i className="token-ridge near" aria-hidden="true" />
    <i className="token-cloud" aria-hidden="true" />
    <div className="token-caption"><strong>{t("tokens.scene.title")}</strong><small>{t("tokens.scene.note")}</small></div>
    {characters && <img className="token-art" src={tigerPlay} alt="" width={705} height={288} draggable={false} />}
  </div>;
}

export function TokenUsageView() {
  const characters = useCharacters();
  const [filter, setFilter] = useState<UsageFilter>("today");
  const [reloadKey, setReloadKey] = useState(0);
  const [showAllModels, setShowAllModels] = useState(false);
  const loaded = useLocalUsage(filter, reloadKey);
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
      <Scene characters={characters} />
      <div className="token-ledger">
        {!loaded && <div className="token-reading" role="status"><Busy label={t("tokens.loading")} /></div>}
        {loaded?.error && <><ErrorMessage error={loaded.error} />{report && <p className="field-help">{t("tokens.error.stale")}</p>}<div className="row-actions inline"><button type="button" onClick={() => setReloadKey(key => key + 1)}>{t("tokens.error.retry")}</button></div></>}
        {report && state === "data" && loaded && <>
          <div className="token-figures">
            <div className="token-main">
              <p className="token-label">{t("tokens.metric.label")} <span className="muted">· {periodLabel(filter, loaded.range)}</span></p>
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
          {report.scanning ? <Busy label={t("tokens.status.reading", { scanned: count(report.filesScanned), pending: count(report.filesPending) })} />
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
