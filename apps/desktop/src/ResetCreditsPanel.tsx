import { useEffect, useRef, useState } from "react";
import { connectResetWeb, assertWebOpened, disconnectResetWeb, openClaudeUsage, openResetWeb, rpc, toApiError } from "./api";
import { ActionFeedback } from "./components";
import { dictionaries, errorCodeText, t } from "./i18n";
import type { MessageKey } from "./i18n";
import { fullTime, useAction, useWindowVisible } from "./state";
import { canUseReset, parseResetInventory, parseResetOperation, resetBlocked, resetScopeKey, resetUseBlock, resetWebOffer, resetWebStale } from "./resets";
import type { ResetCredit, ResetInventory, ResetType, ResetWebStatus } from "./resets";
import type { ApiError } from "./types";

/// How often the open panel re-reads what the service already holds, so updates the service makes by itself (the
/// five-minute web refresh) show up. It never asks the provider for anything: `refresh: false` is a cache read.
const CACHE_POLL_MS = 5_000;

const SCOPE_TEXT: Record<string, MessageKey> = {
  session: "reset.scopeSession", weekly: "reset.scopeWeekly", opus: "reset.scopeOpus", sonnet: "reset.scopeSonnet",
  fable: "reset.scopeFable", cowork: "reset.scopeCowork", omelette: "reset.scopeOmelette", oauthApps: "reset.scopeOauthApps",
  codex: "reset.scopeCodex",
};
const TYPE_TEXT: Record<ResetType, MessageKey> = {
  session: "reset.typeSession", weekly: "reset.typeWeekly", full: "reset.typeFull", fable: "reset.typeFable",
  provider: "reset.typeProvider", unknown: "reset.typeUnknown",
};

/// An identifier this app cannot describe is shown as itself (React escapes it) instead of a generic "other".
function scopeLabel(scope: string): string {
  const key = resetScopeKey(scope);
  return key ? t(SCOPE_TEXT[key]) : t("reset.scopeUnrecognized", { scope });
}
function scopeList(scopes: string[]): string {
  return scopes.length === 0 ? t("reset.scopeOther") : scopes.map(scopeLabel).join(", ");
}
function reasonLabel(token: string): string {
  const key = `reset.reason.${token}` as MessageKey;
  return key in dictionaries.en ? t(key) : t("reset.reasonUnrecognized", { code: token });
}
function typeLabel(credit: ResetCredit): string {
  return t(TYPE_TEXT[credit.resetType]);
}

function webErrorText(web: ResetWebStatus): string | null {
  if (web.errorCode === null) return null;
  return errorCodeText(web.errorCode) ?? t("reset.web.errorUnrecognized", { code: web.errorCode });
}

function failedLookup(previous: ResetInventory | null, error: unknown): ResetInventory | null {
  if (previous?.source !== "web" || previous.web === null) return null;
  return { ...previous, state: "unknown", availableCount: null, redeemableCount: null, quoteId: null,
    web: { ...previous.web, state: "error", errorCode: toApiError(error).code } };
}

export function ResetCreditsPanel({ accountId, name, online, supportsWeb = false, onReload }: {
  accountId: string; name: string; online: boolean;
  /// Claude accounts only. Other providers keep exactly the OAuth-only panel.
  supportsWeb?: boolean; onReload: () => Promise<void>;
}) {
  const [inventory, setInventory] = useState<ResetInventory | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [consent, setConsent] = useState(false);
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const action = useAction();
  const link = useAction();
  const web = useAction();
  const visible = useWindowVisible();
  // Any newer read or use request supersedes an automatic read that is still in flight.
  const epoch = useRef(0);
  const shownAccount = useRef(accountId);
  shownAccount.current = accountId;
  const shown = useRef<ResetInventory | null>(null);
  shown.current = inventory;
  // A cache poll never starts while an explicit read, use or web action is running, so it cannot shadow or cancel one.
  const busy = useRef(false);
  busy.current = loading || action.pending || web.pending;
  useEffect(() => {
    let active = true;
    const mine = ++epoch.current;
    setInventory(null); setLoadError(null); setSelected(null); setConsent(false); setConfirmDisconnect(false); setLoading(online);
    if (!online) return () => { active = false; };
    const current = () => active && epoch.current === mine;
    const accept = (raw: unknown) => {
      const result = parseResetInventory(raw);
      if (result.accountId !== accountId) throw new Error("Account mismatch");
      if (!current()) return;
      setInventory(result); setSelected(null); setConsent(false);
    };
    // Read-only on open: first what the service already has, then a fresh lookup. Nothing here can spend a reset.
    void (async () => {
      try { accept(await rpc<unknown>("resetCredits.read", { accountId, refresh: false })); } catch { /* the fresh lookup below reports failures */ }
      try { accept(await rpc<unknown>("resetCredits.read", { accountId, refresh: true })); }
      catch (error) { if (current()) { setInventory(previous => failedLookup(previous, error)); setLoadError(toApiError(error)); } }
      finally { if (current()) setLoading(false); }
    })();
    return () => { active = false; };
  }, [accountId, online]);
  const webConnected = inventory?.web?.connected === true;
  // Only a connected web read changes by itself, so only it needs to be followed.
  useEffect(() => {
    if (!supportsWeb || !webConnected || !online || !visible) return;
    let active = true;
    let inflight = false;
    const tick = async () => {
      if (inflight || busy.current) return;
      inflight = true;
      const mine = epoch.current;
      try {
        const result = parseResetInventory(await rpc<unknown>("resetCredits.read", { accountId, refresh: false }));
        // An explicit read, use or account change that started meanwhile owns the screen; this older cache read is dropped.
        if (!active || busy.current || epoch.current !== mine || shownAccount.current !== accountId || result.accountId !== accountId) return;
        const previous = shown.current;
        setInventory(result);
        if (previous === null || previous.quoteId !== result.quoteId || previous.source !== result.source) { setSelected(null); setConsent(false); }
      } catch (error) {
        if (active && !busy.current && epoch.current === mine && shownAccount.current === accountId) {
          setInventory(previous => failedLookup(previous, error));
        }
      }
      finally { inflight = false; }
    };
    const timer = setInterval(() => { void tick(); }, CACHE_POLL_MS);
    return () => { active = false; clearInterval(timer); };
  }, [accountId, online, supportsWeb, webConnected, visible]);
  const refresh = () => {
    const mine = ++epoch.current;
    setLoading(false);
    setSelected(null); setConsent(false); setLoadError(null);
    void action.run(async () => {
      try {
        const result = parseResetInventory(await rpc("resetCredits.read", { accountId, refresh: true }));
        if (result.accountId !== accountId) throw new Error("Account mismatch");
        if (shownAccount.current === accountId && epoch.current === mine) setInventory(result);
      } catch (error) {
        // The last web observation stays visible as a reference; an OAuth lookup that failed shows nothing.
        if (shownAccount.current === accountId && epoch.current === mine) setInventory(previous => failedLookup(previous, error));
        throw error;
      }
    });
  };
  const useCredit = () => {
    const credit = inventory?.credits.find(item => item.id === selected);
    if (!consent || !inventory || inventory.accountId !== accountId || !credit || !canUseReset(inventory, credit, Date.now())) return;
    const request = { accountId, quoteId: inventory.quoteId, creditId: credit.id, requestId: crypto.randomUUID(), confirmed: true };
    const mine = ++epoch.current;
    const current = () => shownAccount.current === accountId && epoch.current === mine;
    setLoading(false);
    setSelected(null); setConsent(false);
    void action.run(async () => {
      // Never resend this mutation, even if IPC closes or the user reopens the panel.
      try {
        const outcome = parseResetOperation(await rpc("resetCredits.redeem", request));
        if (outcome.requestId !== request.requestId || outcome.accountId !== accountId) throw new Error("Reset request mismatch");
        if (current()) setInventory(previous => previous ? { ...previous, state: "unknown", availableCount: null, redeemableCount: null, quoteId: null, operation: outcome } : null);
        if (outcome.state === "succeeded") void onReload();
      } catch (error) {
        // Hide old authorization immediately; status recovery is read-only.
        if (current()) setInventory(previous => previous ? { ...previous, quoteId: null } : null);
        try {
          const recovered = parseResetInventory(await rpc("resetCredits.read", { accountId, refresh: false }));
          if (recovered.accountId !== accountId) throw new Error("Account mismatch");
          if (current()) setInventory(recovered);
        }
        catch { /* keep the offer disabled; the service's durable fence remains authoritative */ }
        throw error;
      }
    });
  };
  /// After connecting, show the service's own view (it queues the first web read). The status the native side returns is
  /// only a connection state; the screen waits for the service's inventory before it says anything about offers.
  const connectWeb = () => {
    setConfirmDisconnect(false);
    // A new web binding supersedes any OAuth read that was started before it.
    const mine = ++epoch.current;
    setLoading(false);
    void web.run(async () => {
      const status = await connectResetWeb(accountId);
      if (status.accountId !== accountId) throw new Error("Account mismatch");
      const result = parseResetInventory(await rpc("resetCredits.read", { accountId, refresh: false }));
      if (result.accountId !== accountId) throw new Error("Account mismatch");
      if (shownAccount.current === accountId && epoch.current === mine) { setInventory(result); setLoadError(null); setSelected(null); setConsent(false); }
    });
  };
  const disconnectWeb = () => {
    setConfirmDisconnect(false);
    const mine = ++epoch.current;
    setLoading(false);
    void (async () => {
      // The real read afterwards is the explicit-disconnect fallback to the normal OAuth lookup.
      if (await web.run(async () => {
        const status = await disconnectResetWeb(accountId);
        if (status.accountId !== accountId || status.connected) throw new Error("Disconnect not confirmed");
        // The cleared web observation must not stay on screen as a reference if the OAuth read below fails.
        if (shownAccount.current === accountId && epoch.current === mine) { setInventory(null); setSelected(null); setConsent(false); }
      }) && shownAccount.current === accountId && epoch.current === mine) refresh();
    })();
  };
  const openOfficial = () => { void web.run(async () => { assertWebOpened(await openResetWeb(accountId)); }); };
  const operation = inventory?.operation ?? null;
  const chosen = inventory?.credits.find(item => item.id === selected);
  const state = inventory?.state;
  const isWeb = inventory?.source === "web";
  const now = Date.now();
  const webStatus = inventory?.web ?? null;
  const webStale = inventory !== null && isWeb && resetWebStale(inventory, now);
  // OAuth: offers are shown whenever the service returned them; only a fully known lookup can make one usable.
  // Web: the last observation stays visible (labelled) even when the latest update failed; it is never usable here.
  const offers = isWeb ? (state === "unsupported" ? [] : inventory?.credits ?? [])
    : state === "known" || state === "restricted" ? inventory?.credits ?? [] : [];
  const webFailure = webStatus === null ? null : webErrorText(webStatus);
  const balanceLine = !inventory && loading ? t("common.checking") : state === "unsupported" ? t("reset.unsupported")
    : state === "known" ? t(isWeb ? (webStale ? "reset.web.balanceStale" : "reset.web.balance") : "reset.balance", { available: inventory?.availableCount ?? "—", usable: inventory?.redeemableCount ?? "—" })
    : state === "restricted" ? t("reset.restricted") : t("reset.unknown");
  return <section className="reset-credits" aria-label={t("reset.title")}>
    <h4>{t("reset.title")}</h4>
    <p className="field-help">{t("reset.description")}</p>
    {inventory && state !== "unsupported" && <p className="field-help"><span className="tag">{t(isWeb ? "reset.sourceWeb" : "reset.sourceOauth")}</span></p>}
    <p>{balanceLine}</p>
    {(state === "known" || state === "restricted") && inventory && <small className="muted">{t("reset.observed", { time: fullTime(inventory.observedAt) })}</small>}
    {inventory && state !== "known" && inventory.lookupReason !== null && <p className="field-help">{t("reset.lookupWhy", { reason: reasonLabel(inventory.lookupReason) })}</p>}
    {state === "restricted" && !isWeb && <>
      <p className="warning-text">{t("reset.restrictedHelp")}</p>
      <button type="button" className="link-button" disabled={link.pending} onClick={() => { void link.run(() => openClaudeUsage()); }}>{t("reset.openUsage")}</button>
      <ActionFeedback error={link.error} message={null} />
    </>}
    {operation && <p role="status" className={resetBlocked(operation) ? "warning-text" : "field-help"}>
      {t(operation.state === "succeeded" ? "reset.success" : operation.state === "resolved" ? "reset.resolved"
        : operation.state === "declined" ? "reset.declined" : operation.state === "preparing" ? "reset.preparing" : "reset.uncertain")}
    </p>}
    {supportsWeb && (inventory !== null || loadError !== null) && <div className="reset-web" role="group" aria-label={t("reset.web.title")}>
      <p><strong>{t("reset.web.title")}</strong>{" "}
        <span className={`tag ${webStatus?.state === "ready" ? "accent" : webStatus?.connected ? "warning" : ""}`}>{t(`reset.web.state.${webStatus?.state ?? "disconnected"}` as MessageKey)}</span>
      </p>
      {!webConnected && <p className="field-help">{t("reset.web.intro")}</p>}
      {webStatus !== null && webConnected && <>
        {webStatus.state !== "ready" && <p className={webStatus.state === "syncing" ? "field-help" : "warning-text"}>{t(`reset.web.help.${webStatus.state === "disconnected" ? "error" : webStatus.state}` as MessageKey)}</p>}
        {webFailure !== null && <p className="warning-text">{webFailure}</p>}
        <p className="field-help">
          {webStatus.lastSuccessAt === null ? t("reset.web.neverSucceeded") : t("reset.web.lastSuccess", { time: fullTime(webStatus.lastSuccessAt) })}
          {webStatus.nextRefreshAt !== null && <> · {t("reset.web.next", { time: fullTime(webStatus.nextRefreshAt) })}</>}
        </p>
        <p className="field-help">{t("reset.web.keepRunning")}</p>
      </>}
      <div className="setup-actions">
        <button type="button" disabled={!online || web.pending || action.pending} onClick={connectWeb}>{t(webConnected ? "reset.web.reconnect" : "reset.web.connect")}</button>
        {webConnected && <button type="button" disabled={!online || web.pending} onClick={openOfficial}>{t("reset.web.open")}</button>}
        {webConnected && !confirmDisconnect && <button type="button" disabled={!online || web.pending || action.pending} onClick={() => setConfirmDisconnect(true)}>{t("reset.web.disconnect")}</button>}
      </div>
      {confirmDisconnect && <div className="setup-body" role="group" aria-label={t("reset.web.disconnect")}>
        <p className="warning-text">{t("reset.web.disconnectConfirm")}</p>
        <div className="setup-actions">
          <button type="button" onClick={() => setConfirmDisconnect(false)}>{t("reset.cancel")}</button>
          <button type="button" className="primary" disabled={!online || web.pending || action.pending} onClick={disconnectWeb}>{t("reset.web.disconnectYes")}</button>
        </div>
      </div>}
      <ActionFeedback error={web.error} message={null} />
    </div>}
    <button type="button" disabled={!online || action.pending || web.pending || state === "unsupported"} onClick={refresh}>
      {t(action.pending ? "common.checking" : "reset.refresh")}
    </button>
    {inventory && offers.map(credit => {
      const block = resetUseBlock(inventory, credit, now);
      const webOffer = isWeb ? resetWebOffer(inventory, credit, now) : null;
      return <div className="reset-credit" key={credit.id}>
        <p>
          <span className={`tag ${credit.resetType === "fable" ? "accent" : credit.resetType === "unknown" ? "warning" : ""}`}>{typeLabel(credit)}</span>
          {" "}{t("reset.credit", { count: credit.remainingCount })}
          {webOffer !== null && <>{" "}<span className={`tag ${webOffer === "available" ? "accent" : "warning"}`}>{t(webOffer === "available" ? "reset.web.offerAvailable" : webOffer === "stale" ? "reset.web.offerStale" : "reset.web.offerUnavailable")}</span></>}
        </p>
        <p>{t("reset.clears", { scopes: scopeList(credit.clears) })}</p>
        {credit.resetType === "fable" && <p className="field-help">{t("reset.typeFableHelp")}</p>}
        {credit.blocking.length > 0 && <p className="field-help">{t("reset.blockedBy", { scopes: scopeList(credit.blocking) })}</p>}
        <p className="field-help">
          {credit.startsAt !== null && <>{t("reset.startsAt", { time: fullTime(credit.startsAt) })} · </>}
          {credit.expiresAt === null ? t("reset.expiryUnknown") : t("reset.expires", { time: fullTime(credit.expiresAt) })}
          {credit.cooldownUntil !== null && credit.cooldownUntil > now && <> · {t("reset.cooldown", { time: fullTime(credit.cooldownUntil) })}</>}
        </p>
        {credit.unavailableReason !== null && <p className="warning-text">{reasonLabel(credit.unavailableReason)}</p>}
        {block === "unrecognized" && credit.unavailableReason === null && <p className="warning-text">{t("reset.blockUnrecognized")}</p>}
        {block === "stale" && <p className="field-help">{t("reset.blockStale")}</p>}
        {isWeb ? <p className="field-help">{t("reset.web.useOfficial")}</p>
          : state === "known"
            ? <button type="button" disabled={!online || action.pending || block !== null}
              onClick={() => { setSelected(credit.id); setConsent(false); }}>{t("reset.use")}</button>
            : <p className="field-help">{t("reset.observationOnly")}</p>}
      </div>;
    })}
    {chosen && inventory && !isWeb && <div className="setup-body" role="group" aria-label={t("reset.confirmTitle")}>
      <p><strong>{t("reset.confirmAccount", { account: name })}</strong></p>
      <p>{t("reset.confirmType", { type: typeLabel(chosen) })}</p>
      <p>{t("reset.confirmScope", { scope: scopeList(chosen.clears) })}</p>
      {chosen.resetType === "fable" && <p className="field-help">{t("reset.typeFableHelp")}</p>}
      <p className="warning-text">{t("reset.confirmWarning")}</p>
      <label><input type="checkbox" checked={consent} onChange={event => setConsent(event.target.checked)} /> {t("reset.consent")}</label>
      <div className="setup-actions">
        <button type="button" onClick={() => { setSelected(null); setConsent(false); }}>{t("reset.cancel")}</button>
        <button type="button" className="primary" disabled={!online || !consent || action.pending || !canUseReset(inventory, chosen, Date.now())} onClick={useCredit}>{t("reset.confirmUse")}</button>
      </div>
    </div>}
    <ActionFeedback error={action.error ?? loadError} message={null} />
  </section>;
}
