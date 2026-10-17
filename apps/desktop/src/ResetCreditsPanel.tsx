import { useEffect, useState } from "react";
import { rpc, toApiError } from "./api";
import { ActionFeedback } from "./components";
import { t } from "./i18n";
import { fullTime, useAction } from "./state";
import { canUseReset, parseResetInventory, parseResetOperation, resetBlocked } from "./resets";
import type { ResetInventory } from "./resets";
import type { ApiError } from "./types";

function scopeLabel(scope: string): string {
  if (scope === "anthropic:5h") return t("reset.scopeSession");
  if (scope === "anthropic:7d") return t("reset.scopeWeekly");
  if (scope === "anthropic:7d:opus") return t("reset.scopeOpus");
  if (scope === "anthropic:7d:sonnet") return t("reset.scopeSonnet");
  if (scope === "openai-codex:rate_limits") return t("reset.scopeCodex");
  return t("reset.scopeOther");
}

export function ResetCreditsPanel({ accountId, name, online, onReload }: {
  accountId: string; name: string; online: boolean; onReload: () => Promise<void>;
}) {
  const [inventory, setInventory] = useState<ResetInventory | null>(null);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [consent, setConsent] = useState(false);
  const action = useAction();
  useEffect(() => {
    let active = true;
    setInventory(null); setLoadError(null); setSelected(null); setConsent(false);
    if (online) void rpc<unknown>("resetCredits.read", { accountId, refresh: false }).then(raw => {
      const result = parseResetInventory(raw);
      if (result.accountId !== accountId) throw new Error("Account mismatch");
      if (active) setInventory(result);
    }).catch(error => { if (active) setLoadError(toApiError(error)); });
    return () => { active = false; };
  }, [accountId, online]);
  const refresh = () => {
    setSelected(null); setConsent(false); setLoadError(null);
    void action.run(async () => {
      try {
        const result = parseResetInventory(await rpc("resetCredits.read", { accountId, refresh: true }));
        if (result.accountId !== accountId) throw new Error("Account mismatch");
        setInventory(result);
      } catch (error) { setInventory(null); throw error; }
    });
  };
  const useCredit = () => {
    const credit = inventory?.credits.find(item => item.id === selected);
    if (!consent || !inventory || !credit || !canUseReset(inventory, credit, Date.now())) return;
    const request = { accountId, quoteId: inventory.quoteId, creditId: credit.id, requestId: crypto.randomUUID(), confirmed: true };
    setSelected(null); setConsent(false);
    void action.run(async () => {
      // Never resend this mutation, even if IPC closes or the user reopens the panel.
      try {
        const outcome = parseResetOperation(await rpc("resetCredits.redeem", request));
        if (outcome.requestId !== request.requestId || outcome.accountId !== accountId) throw new Error("Reset request mismatch");
        setInventory(previous => previous ? { ...previous, state: "unknown", availableCount: null, redeemableCount: null, quoteId: null, operation: outcome } : null);
        if (outcome.state === "succeeded") void onReload();
      } catch (error) {
        // Hide old authorization immediately; status recovery is read-only.
        setInventory(previous => previous ? { ...previous, quoteId: null } : null);
        try {
          const recovered = parseResetInventory(await rpc("resetCredits.read", { accountId, refresh: false }));
          if (recovered.accountId !== accountId) throw new Error("Account mismatch");
          setInventory(recovered);
        }
        catch { /* keep the offer disabled; the service's durable fence remains authoritative */ }
        throw error;
      }
    });
  };
  const operation = inventory?.operation ?? null;
  const chosen = inventory?.credits.find(item => item.id === selected);
  return <section className="reset-credits" aria-label={t("reset.title")}>
    <h4>{t("reset.title")}</h4>
    <p className="field-help">{t("reset.description")}</p>
    <p>{inventory?.state === "unsupported" ? t("reset.unsupported") : inventory?.state === "known"
      ? t("reset.balance", { available: inventory.availableCount ?? 0, usable: inventory.redeemableCount ?? 0 }) : t("reset.unknown")}</p>
    {inventory?.state === "known" && <small className="muted">{t("reset.observed", { time: fullTime(inventory.observedAt) })}</small>}
    {operation && <p role="status" className={resetBlocked(operation) ? "warning-text" : "field-help"}>
      {t(operation.state === "succeeded" ? "reset.success" : operation.state === "resolved" ? "reset.resolved"
        : operation.state === "declined" ? "reset.declined" : operation.state === "preparing" ? "reset.preparing" : "reset.uncertain")}
    </p>}
    <button type="button" disabled={!online || action.pending || inventory?.state === "unsupported"} onClick={refresh}>
      {t(action.pending ? "common.checking" : "reset.refresh")}
    </button>
    {inventory?.state === "known" && inventory.credits.map(credit => <div className="reset-credit" key={credit.id}>
      <p>{t("reset.credit", { count: credit.remainingCount })} · {credit.clears.map(scopeLabel).join(", ") || t("reset.scopeOther")}</p>
      <p className="field-help">{credit.expiresAt === null ? t("reset.expiryUnknown") : t("reset.expires", { time: fullTime(credit.expiresAt) })}</p>
      <button type="button" disabled={!online || action.pending || !canUseReset(inventory, credit, Date.now())}
        onClick={() => { setSelected(credit.id); setConsent(false); }}>{t("reset.use")}</button>
    </div>)}
    {chosen && inventory && <div className="setup-body" role="group" aria-label={t("reset.confirmTitle")}>
      <p><strong>{t("reset.confirmAccount", { account: name })}</strong></p>
      <p>{t("reset.confirmScope", { scope: chosen.clears.map(scopeLabel).join(", ") || t("reset.scopeOther") })}</p>
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
