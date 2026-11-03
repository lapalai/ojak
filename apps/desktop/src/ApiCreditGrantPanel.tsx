import { useEffect, useState } from "react";
import { ActionFeedback, Badge, Busy, Switch, Tip } from "./components";
import { billingAccounts, billingFormFrom, billingPatch, estimateView, grantFormFrom, grantPatch, grantView, proactiveBlockKey } from "./api-credit-grant";
import { allocationModeOf } from "./allocation-mode";
import type { BillingFormError, GrantFormError } from "./api-credit-grant";
import { apiAccounts, apiCreditUsage } from "./api-credit";
import { fullTime, privacyText, timeZoneLabel, useAction } from "./state";
import { t } from "./i18n";
import { planLabel } from "./plan";
import type { MessageKey } from "./i18n";
import { usdLimit, usdUsed } from "./limits";
import type { BillingCycle, Policy, Snapshot } from "./types";

const ERROR_KEYS: Record<GrantFormError, MessageKey> = {
  budget: "grant.error.budget", start: "grant.error.start", expiry: "grant.error.expiry", range: "grant.error.range",
  past: "grant.error.past", tooLong: "grant.error.tooLong", confirm: "grant.error.confirm",
  estimateNone: "grant.error.estimateNone", estimateAnnual: "grant.error.estimateAnnual", estimatePassed: "grant.error.estimatePassed",
  estimateAccount: "grant.error.estimateAccount", estimateConfirm: "grant.error.estimateConfirm",
};
const BILLING_ERROR_KEYS: Record<BillingFormError, MessageKey> = {
  account: "billing.error.account", date: "billing.error.date", past: "billing.error.past", tooFar: "billing.error.tooFar",
};
const ESTIMATE_BLOCK_KEYS = { NONE: "grant.estimate.none", ANNUAL: "grant.estimate.annual", DATE_PASSED: "grant.estimate.passed", ACCOUNT_INVALID: "grant.estimate.account" } as const;
const STATE_TONE = { unknown: "neutral", upcoming: "neutral", active: "good", exhausted: "warning", expired: "warning" } as const;

/// 사용자가 직접 확인해 입력하는 API 크레딧 사용 가능 금액·기간과, 그 크레딧을 만료 전에 새 omp 세션에 미리 쓸지 정하는 설정.
/// 금액·기간은 Console에서 읽은 값이 아니라 사용자가 입력한 값이고, 사용액은 Ojak이 보낸 요청의 추정치다. 등록한 모든 키가 하나의 풀을 함께 쓴다.
/// 상태 판단(미리 쓰기 가능 여부 포함)은 서비스의 `snapshot.apiCreditGrant`만 믿고 여기서 다시 계산하지 않는다.
/// 만료는 정확한 날짜를 직접 입력하거나, 직접 저장한 월 결제일에서 서비스가 계산한 추정을 별도 동의로 쓴다. 연 결제·지난 결제일은 추정하지 않고 이유를 보여 준다.
/// 결제 정보 저장은 이미 확인한 크레딧의 만료·사용액을 바꾸지 않는다. 입력 중인 값은 저장된 값이 바뀔 때만 다시 채우고 3초 조회로는 지우지 않는다.
export function ApiCreditGrantPanel({ snapshot, online, masked = false, update }: { snapshot: Snapshot; online: boolean; masked?: boolean; update: (patch: Record<string, unknown>) => Promise<Policy> }) {
  const { policy } = snapshot;
  const action = useAction();
  const grant = policy.apiCreditGrant ?? null;
  const view = grantView(snapshot.apiCreditGrant);
  const keys = apiAccounts(snapshot.accounts, "anthropic");
  const mode = allocationModeOf(policy.allocationMode);
  const billing = policy.apiCreditBilling ?? null;
  const eligible = billingAccounts(snapshot.accounts);
  const [billingForm, setBillingForm] = useState(() => billingFormFrom(billing, eligible));
  const [billingError, setBillingError] = useState<BillingFormError | null>(null);
  const [form, setForm] = useState(() => grantFormFrom(grant, snapshot.apiCreditGrant, snapshot.generatedAt));
  const [formError, setFormError] = useState<GrantFormError | null>(null);
  // 저장된 입력이 바뀌면(다른 곳에서 바꿨거나 지웠을 때) 폼을 그 값으로 다시 채운다. 입력 중에는 건드리지 않는다.
  useEffect(() => { setForm(grantFormFrom(grant, snapshot.apiCreditGrant, snapshot.generatedAt)); setFormError(null); }, [grant?.id, grant?.confirmedAt]);
  // 결제 정보도 저장된 값이 바뀔 때만 다시 채운다. 3초 주기 조회로는 입력 중인 값이 지워지지 않는다.
  useEffect(() => { setBillingForm(billingFormFrom(billing, eligible)); setBillingError(null); }, [billing?.accountId, billing?.cycle, billing?.nextBillingAt, billing?.updatedAt]);
  const busy = !online || action.pending;
  const monthly = apiCreditUsage(snapshot.accounts, snapshot.generatedAt, snapshot.apiCreditRemoved);
  const blockKey = proactiveBlockKey(view.proactive.blockedBy);

  function save() {
    const result = grantPatch(form, Date.now(), snapshot.apiCreditGrant?.billing);
    if (!result.ok) { setFormError(result.error); return; }
    setFormError(null);
    void action.run(() => update({ apiCreditGrant: result.patch }), () => t("grant.saved"));
  }
  function saveBilling() {
    const result = billingPatch(billingForm, eligible, Date.now());
    if (!result.ok) { setBillingError(result.error); return; }
    setBillingError(null);
    void action.run(() => update({ apiCreditBilling: result.patch }), () => t("billing.saved"));
  }
  const estimate = estimateView(snapshot.apiCreditGrant?.billing, snapshot.generatedAt);
  const estimating = form.expiryMode === "billing-estimate";
  const accountName = (id: string) => { const account = snapshot.accounts.find(item => item.id === id); return account ? privacyText(account.email ?? account.label, masked) : t("billing.accountMissing"); };
  const estimated = grant?.expirySource === "billing-estimate";
  return <div className="panel api-grant" aria-labelledby="api-grant-title">
    <header><h2 id="api-grant-title">{t("grant.title")}</h2><Tip tip={t("grant.tip")} label={t("grant.tipLabel")} /><span className="aside"><Badge tone={STATE_TONE[view.state]}>{t(`grant.state.${view.state}`)}</Badge></span></header>
    <div className="api-grant-body">
      <p className="field-help">{t("grant.manualCaveat")}</p>
      {keys.length === 0 && <p className="gate-reason" role="status">{t("policy.apiCreditNoKey")}</p>}
      {grant && <ul className="grant-facts" aria-label={t("grant.factsAria")}>
        {view.state === "unknown" && <li>{t("grant.state.unknownDesc")}</li>}
        {view.remainingUsd !== null && view.budgetUsd !== null && <li>{t("grant.remaining", { remaining: usdUsed(view.remainingUsd), budget: usdLimit(view.budgetUsd) })}</li>}
        {view.state === "active" && view.perDayUsd !== null && <li>{t("grant.perDay", { amount: usdUsed(view.perDayUsd) })}</li>}
        {view.startsAt !== null && view.state === "upcoming" && <li>{t("grant.starts", { time: fullTime(view.startsAt) })}</li>}
        {view.expiresAt !== null && <li>{t(view.state === "expired" ? "grant.expired" : "grant.expires", { time: fullTime(view.expiresAt) })}{estimated && <> <span className="tag warning">{t("grant.estimateTag")}</span></>}</li>}
        {estimated && <li>{t("grant.estimateNote", { account: accountName(snapshot.apiCreditGrant?.billingEstimate?.accountId ?? grant?.billingEstimate?.accountId ?? billing?.accountId ?? "") })}</li>}
        {estimated && snapshot.apiCreditGrant?.estimateDrift && <li className="warn">{t("grant.estimateDrift")}</li>}
        {view.state === "exhausted" && <li>{t("grant.exhausted")}</li>}
        {view.state === "expired" && <li>{t("grant.expiredNote")}</li>}
        {view.unmetered && <li className="warn">{t("grant.unmetered", { count: view.unmeteredRequests })}</li>}
      </ul>}
      {!grant && <p className="field-help">{t("grant.none")}</p>}
      <form className="grant-form billing-form" aria-labelledby="billing-title" onSubmit={event => { event.preventDefault(); if (!busy) saveBilling(); }}>
        <h3 id="billing-title" className="grant-subtitle">{t("billing.title")}<Tip tip={t("billing.tip")} label={t("billing.tipLabel")} /></h3>
        <p className="field-help">{t("billing.manual")}</p>
        {eligible.length === 0 && <p className="gate-reason" role="status">{t("billing.noAccount")}</p>}
        <label>{t("billing.account")}<select value={billingForm.accountId} aria-label={t("billing.accountAria")} aria-invalid={billingError === "account"} disabled={busy || eligible.length === 0} onChange={event => setBillingForm({ ...billingForm, accountId: event.target.value })}>
          <option value="">{t("billing.accountPick")}</option>
          {eligible.map(account => <option key={account.id} value={account.id}>{privacyText(account.email ?? account.label, masked)}{account.plan ? ` · ${planLabel(account.provider, account.plan)}` : ""}</option>)}
        </select></label>
        <label>{t("billing.cycle")}<select value={billingForm.cycle} aria-label={t("billing.cycleAria")} disabled={busy} onChange={event => setBillingForm({ ...billingForm, cycle: event.target.value as BillingCycle })}>
          <option value="monthly">{t("billing.cycle.monthly")}</option>
          <option value="annual">{t("billing.cycle.annual")}</option>
        </select></label>
        <label>{t("billing.next")}<input type="datetime-local" value={billingForm.nextBillingAt} aria-label={t("billing.nextAria")} aria-invalid={billingError === "date" || billingError === "past" || billingError === "tooFar"} disabled={busy} onChange={event => setBillingForm({ ...billingForm, nextBillingAt: event.target.value })} /></label>
        <p className="field-help">{t("billing.zone", { zone: timeZoneLabel() })}</p>
        {billingError && <p className="gate-reason" role="alert">{t(BILLING_ERROR_KEYS[billingError])}</p>}
        {billing && <p className="field-help" role="status">{t("billing.saved.line", { account: accountName(billing.accountId), cycle: t(billing.cycle === "annual" ? "billing.cycle.annual" : "billing.cycle.monthly"), time: fullTime(billing.nextBillingAt) })}</p>}
        <div className="grant-actions">
          <button type="submit" className="primary" disabled={busy || eligible.length === 0}>{action.pending ? <Busy /> : t("billing.save")}</button>
          {billing && <button type="button" disabled={busy} onClick={() => { void action.run(() => update({ apiCreditBilling: null }), () => t("billing.cleared")); }}>{t("billing.clear")}</button>}
        </div>
      </form>
      <form className="grant-form" onSubmit={event => { event.preventDefault(); if (!busy) save(); }}>
        <h3 className="grant-subtitle">{t("grant.formTitle")}</h3>
        <label>{t("grant.budget")}<span className="grant-input"><input type="number" min={0.01} step={0.01} inputMode="decimal" value={form.budget} aria-label={t("grant.budgetAria")} aria-invalid={formError === "budget"} disabled={busy} onChange={event => setForm({ ...form, budget: event.target.value })} />USD</span></label>
        <label>{t("grant.starts.label")}<input type="datetime-local" value={form.startsAt} aria-label={t("grant.startsAria")} aria-invalid={formError === "start" || formError === "range"} disabled={busy} onChange={event => setForm({ ...form, startsAt: event.target.value })} /></label>
        <fieldset className="grant-expiry" disabled={busy}>
          <legend>{t("grant.expiry.legend")}</legend>
          <label className="grant-radio"><input type="radio" name="grant-expiry-mode" checked={estimating} disabled={estimate.blockedBy !== null && !estimating} onChange={() => setForm({ ...form, expiryMode: "billing-estimate" })} /><span>{t("grant.expiry.estimate")}</span></label>
          <label className="grant-radio"><input type="radio" name="grant-expiry-mode" checked={!estimating} onChange={() => setForm({ ...form, expiryMode: "exact", estimateConfirmedFor: null })} /><span>{t("grant.expiry.exact")}</span></label>
        </fieldset>
        {estimating ? <>
          {estimate.blockedBy === null
            ? <p className="grant-estimate" role="status"><strong>{fullTime(estimate.expiresAt)}</strong> <span className="tag warning">{t("grant.estimateTag")}</span><br />{t("grant.estimate.basis", { zone: timeZoneLabel() })}</p>
            : <p className="gate-reason" role="status">{t(ESTIMATE_BLOCK_KEYS[estimate.blockedBy])}</p>}
          {estimate.blockedBy === null && <label className="grant-confirm"><input type="checkbox" checked={form.estimateConfirmedFor === estimate.expiresAt} onChange={event => setForm({ ...form, estimateConfirmedFor: event.target.checked ? estimate.expiresAt : null })} /><span>{t("grant.estimate.confirm")}</span></label>}
        </> : <>
          {estimate.blockedBy !== null && <p className="field-help">{t(ESTIMATE_BLOCK_KEYS[estimate.blockedBy])}</p>}
          <label>{t("grant.expires.label")}<input type="datetime-local" value={form.expiresAt} aria-label={t("grant.expiresAria")} aria-invalid={formError === "expiry" || formError === "range" || formError === "past" || formError === "tooLong"} onChange={event => setForm({ ...form, expiresAt: event.target.value })} /></label>
        </>}
        <label className="grant-confirm"><input type="checkbox" checked={form.confirmed} disabled={busy} onChange={event => setForm({ ...form, confirmed: event.target.checked })} /><span>{t(estimating ? "grant.confirm.balance" : "grant.confirm")}</span></label>
        {formError && <p className="gate-reason" role="alert">{t(ERROR_KEYS[formError])}</p>}
        <p className="field-help">{t("grant.safety")}</p>
        <div className="grant-actions">
          <button type="submit" className="primary" disabled={busy || keys.length === 0}>{action.pending ? <Busy /> : t("grant.save")}</button>
          {grant && <button type="button" disabled={busy} onClick={() => { void action.run(() => update({ apiCreditGrant: null }), () => t("grant.cleared")); }}>{t("grant.clear")}</button>}
        </div>
      </form>
      <div className="rule grant-balance">
        <div className="t">{t("balance.title")} <Tip tip={t("balance.tip")} label={t("balance.tipLabel")} /></div>
        <div className="d">{t("balance.desc")}</div>
        <div className="ctl"><Switch checked={Boolean(policy.balanceApiCredit)} disabled={busy || (!policy.balanceApiCredit && keys.length === 0)} label={t("balance.title")} onChange={balanceApiCredit => { void action.run(() => update({ balanceApiCredit }), () => balanceApiCredit ? t("balance.on") : t("balance.off")); }} /></div>
      </div>
      {!policy.useApiCreditAfterLimit && <p className="field-help">{t("balance.needPaid")}</p>}
      {mode !== "expiry-balanced" && <p className="field-help">{t("balance.needMode")}</p>}
      {policy.balanceApiCredit && <p className={view.proactive.allowed ? "field-help" : "gate-reason"} role="status">{view.proactive.allowed ? t("balance.allowed") : t("balance.blocked", { reason: blockKey ? t(blockKey) : t("balance.block.unknown") })}</p>}
      {snapshot.apiCreditGrant?.monthlyLimitReached && <p className="gate-reason" role="status">{t("balance.block.MONTHLY_LIMIT")}</p>}
      {keys.length > 0 && <p className="field-help">{t("policy.apiCreditUsage", { amount: `${usdUsed(monthly.usedUsd)} / ${usdLimit(policy.apiCreditMonthlyLimitUsd ?? 200)}` })}</p>}
      <ActionFeedback error={action.error} message={action.message} />
    </div>
  </div>;
}
