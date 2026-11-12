import { useEffect, useState } from "react";
import { ActionFeedback, Badge, Busy, Switch, Tip } from "./components";
import { billingAccounts, billingFormFrom, billingPatch, estimateView, grantFormFrom, grantPatch, poolOfKey, proactiveBlockKey, ungroupedKeys } from "./api-credit-grant";
import { allocationModeOf } from "./allocation-mode";
import type { BillingFormError, GrantFormError } from "./api-credit-grant";
import { DEFAULT_API_CREDIT_LIMIT_USD, apiAccounts, apiCreditSpend, apiCreditUsage } from "./api-credit";
import { fullTime, privacyText, timeZoneLabel, useAction } from "./state";
import { t } from "./i18n";
import { planLabel } from "./plan";
import type { MessageKey } from "./i18n";
import { usdLimit, usdUsed } from "./limits";
import type { BillingCycle, Policy, Snapshot } from "./types";

const ERROR_KEYS: Record<GrantFormError, MessageKey> = {
  keys: "grant.error.keys", budget: "grant.error.budget", start: "grant.error.start", expiry: "grant.error.expiry", range: "grant.error.range",
  past: "grant.error.past", tooLong: "grant.error.tooLong", confirm: "grant.error.confirm",
  estimateNone: "grant.error.estimateNone", estimateAnnual: "grant.error.estimateAnnual", estimatePassed: "grant.error.estimatePassed",
  estimateAccount: "grant.error.estimateAccount", estimateConfirm: "grant.error.estimateConfirm",
};
const BILLING_ERROR_KEYS: Record<BillingFormError, MessageKey> = {
  account: "billing.error.account", date: "billing.error.date", past: "billing.error.past", tooFar: "billing.error.tooFar",
};
const ESTIMATE_BLOCK_KEYS = { NONE: "grant.estimate.none", ANNUAL: "grant.estimate.annual", DATE_PASSED: "grant.estimate.passed", ACCOUNT_INVALID: "grant.estimate.account" } as const;
const STATE_TONE = { upcoming: "neutral", active: "good", exhausted: "warning", expired: "warning" } as const;

/// 사용자가 직접 확인해 입력하는 API 크레딧 풀(같은 Console 조직 잔액을 나눠 쓰는 키 묶음 + 금액·기간)과, 그 크레딧을 만료 전에 새 omp 세션에 미리 쓸지 정하는 설정.
/// 금액·기간은 Console에서 읽은 값이 아니라 사용자가 입력한 값이고, 사용액은 Ojak이 보낸 요청의 추정치다. 키 하나는 풀 하나에만 속하고, 어느 풀에도 없는 키는 한도 뒤에만 쓴다.
/// 상태 판단(미리 쓰기 가능 여부 포함)은 서비스의 `snapshot.apiCreditGrants`만 믿고 여기서 다시 계산하지 않는다.
/// 풀을 수정하면 같은 id로 저장해 사용액을 이어 가고, 새 풀은 id 없이 저장해 0부터 센다.
/// 만료는 정확한 날짜를 직접 입력하거나, 직접 저장한 월 결제일에서 서비스가 계산한 추정을 별도 동의로 쓴다. 연 결제·지난 결제일은 추정하지 않고 이유를 보여 준다.
/// 결제 정보 저장은 이미 확인한 풀의 만료·사용액을 바꾸지 않는다. 입력 중인 값은 3초 조회로 지우지 않고, 수정 중인 풀·결제 정보가 사라졌을 때만 새 입력으로 되돌린다.
export function ApiCreditGrantPanel({ snapshot, online, masked = false, update }: { snapshot: Snapshot; online: boolean; masked?: boolean; update: (patch: Record<string, unknown>) => Promise<Policy> }) {
  const { policy } = snapshot;
  const action = useAction();
  const grants = policy.apiCreditGrants ?? [];
  const billings = policy.apiCreditBillings ?? [];
  const grantStatuses = snapshot.apiCreditGrants ?? [];
  const billingStatuses = snapshot.apiCreditBillings ?? [];
  const keys = apiAccounts(snapshot.accounts, "anthropic");
  const keyIds = keys.map(key => key.id);
  const mode = allocationModeOf(policy.allocationMode);
  const eligible = billingAccounts(snapshot.accounts);
  const freeEligible = eligible.filter(account => !billings.some(billing => billing.accountId === account.id));
  const [billingForm, setBillingForm] = useState(() => billingFormFrom(null, freeEligible));
  const [editingBilling, setEditingBilling] = useState<string | null>(null);
  const [billingError, setBillingError] = useState<BillingFormError | null>(null);
  const [form, setForm] = useState(() => grantFormFrom(null, billingStatuses, snapshot.generatedAt, keyIds));
  const [formError, setFormError] = useState<GrantFormError | null>(null);
  const editingExists = form.id !== null && grants.some(grant => grant.id === form.id);
  const billingEditingExists = editingBilling !== null && billings.some(billing => billing.accountId === editingBilling);
  // 수정하던 풀이 다른 곳에서 지워졌으면 새 풀 입력으로 되돌린다. 그 밖에는 3초 조회로 입력 중인 값을 건드리지 않는다.
  useEffect(() => { if (form.id !== null && !editingExists) { setForm(grantFormFrom(null, billingStatuses, snapshot.generatedAt, keyIds)); setFormError(null); } }, [form.id, editingExists]);
  useEffect(() => { if (editingBilling !== null && !billingEditingExists) { setEditingBilling(null); setBillingForm(billingFormFrom(null, freeEligible)); setBillingError(null); } }, [editingBilling, billingEditingExists]);
  const busy = !online || action.pending;
  const monthly = apiCreditUsage(snapshot.accounts, snapshot.generatedAt, snapshot.apiCreditRemoved);
  const accountName = (id: string) => { const account = snapshot.accounts.find(item => item.id === id); return account ? privacyText(account.email ?? account.label, masked) : t("billing.accountMissing"); };
  const ungrouped = ungroupedKeys(keys, grants);
  const estimate = estimateView(billingStatuses.find(billing => billing.accountId === form.billingAccountId), snapshot.generatedAt);
  const estimating = form.expiryMode === "billing-estimate";
  const editing = form.id !== null;

  function save() {
    const result = grantPatch(form, Date.now(), billingStatuses);
    if (!result.ok) { setFormError(result.error); return; }
    setFormError(null);
    void action.run(() => update({ apiCreditGrant: result.patch }), () => t(editing ? "grant.saved.edit" : "grant.saved")).then(ok => { if (ok) setForm(grantFormFrom(null, billingStatuses, snapshot.generatedAt, keyIds)); });
  }
  function saveBilling() {
    const result = billingPatch(billingForm, eligible, Date.now());
    if (!result.ok) { setBillingError(result.error); return; }
    setBillingError(null);
    void action.run(() => update({ apiCreditBilling: result.patch }), () => t("billing.saved")).then(ok => {
      if (!ok) return;
      setEditingBilling(null);
      setBillingForm(billingFormFrom(null, freeEligible.filter(account => account.id !== result.patch.accountId)));
    });
  }
  return <div className="panel api-grant" aria-labelledby="api-grant-title">
    <header><h2 id="api-grant-title">{t("grant.title")}</h2><Tip tip={t("grant.tip")} label={t("grant.tipLabel")} /></header>
    <div className="api-grant-body">
      <p className="field-help">{t("grant.manualCaveat")}</p>
      {keys.length === 0 && <p className="gate-reason" role="status">{t("policy.apiCreditNoKey")}</p>}
      {grants.length === 0 && <p className="field-help">{t("grant.none")}</p>}
      {grants.length > 0 && <ul className="grant-pools" aria-label={t("grant.poolsAria")}>
        {grants.map(grant => {
          const status = grantStatuses.find(item => item.id === grant.id);
          const estimated = grant.expirySource === "billing-estimate";
          const blockKey = proactiveBlockKey(status?.proactive.blockedBy);
          return <li key={grant.id} className="grant-pool">
            <div className="grant-pool-head">
              <strong>{t("grant.pool.keys", { keys: grant.accountIds.map(id => keyIds.includes(id) ? accountName(id) : t("grant.keys.removed")).join(", ") })}</strong>
              {status && <Badge tone={STATE_TONE[status.state]}>{t(`grant.state.${status.state}`)}</Badge>}
            </div>
            <ul className="grant-facts" aria-label={t("grant.factsAria")}>
              {status && <li>{t("grant.remaining", { remaining: usdUsed(status.remainingUsd), budget: usdLimit(status.budgetUsd) })}</li>}
              {status?.state === "active" && status.perDayUsd !== null && <li>{t("grant.perDay", { amount: usdUsed(status.perDayUsd) })}</li>}
              {status?.state === "upcoming" && <li>{t("grant.starts", { time: fullTime(status.startsAt) })}</li>}
              <li>{t(status?.state === "expired" ? "grant.expired" : "grant.expires", { time: fullTime(grant.expiresAt) })}{estimated && <> <span className="tag warning">{t("grant.estimateTag")}</span></>}</li>
              {estimated && <li>{t("grant.estimateNote", { account: accountName(status?.billingEstimate?.accountId ?? grant.billingEstimate?.accountId ?? "") })}</li>}
              {estimated && status?.estimateDrift && <li className="warn">{t("grant.estimateDrift")}</li>}
              {status?.state === "exhausted" && <li>{t("grant.exhausted")}</li>}
              {status?.state === "expired" && <li>{t("grant.expiredNote")}</li>}
              {status?.unmetered && <li className="warn">{t("grant.unmetered", { count: status.unmeteredRequests })}</li>}
              {policy.balanceApiCredit && status && <li className={status.proactive.allowed ? undefined : "warn"}>{status.proactive.allowed ? t("balance.allowed") : t("balance.blocked", { reason: blockKey ? t(blockKey) : t("balance.block.unknown") })}</li>}
            </ul>
            <div className="grant-actions">
              <button type="button" disabled={busy} onClick={() => { setForm(grantFormFrom(grant, billingStatuses, snapshot.generatedAt, keyIds)); setFormError(null); }}>{t("grant.edit")}</button>
              <button type="button" disabled={busy} onClick={() => { void action.run(() => update({ apiCreditGrantRemove: grant.id }), () => t("grant.cleared")); }}>{t("grant.remove")}</button>
            </div>
          </li>;
        })}
      </ul>}
      {grants.length > 0 && ungrouped.length > 0 && <p className="field-help" role="status">{t("grant.keys.ungrouped", { keys: ungrouped.map(key => privacyText(key.email ?? key.label, masked)).join(", ") })}</p>}
      <form className="grant-form" aria-labelledby="grant-form-title" onSubmit={event => { event.preventDefault(); if (!busy) save(); }}>
        <h3 id="grant-form-title" className="grant-subtitle">{t(editing ? "grant.form.edit" : "grant.form.new")}</h3>
        <fieldset className="grant-keys" disabled={busy}>
          <legend>{t("grant.keys.legend")}</legend>
          {keys.map(key => {
            const other = poolOfKey(grants, key.id, form.id);
            return <label key={key.id} className="grant-radio"><input type="checkbox" checked={form.accountIds.includes(key.id)} disabled={other !== null} aria-invalid={formError === "keys"} onChange={event => setForm({ ...form, accountIds: event.target.checked ? [...form.accountIds, key.id] : form.accountIds.filter(id => id !== key.id) })} /><span>{privacyText(key.email ?? key.label, masked)}</span>{other !== null && <span className="tag">{t("grant.keys.inOtherPool")}</span>}</label>;
          })}
        </fieldset>
        <p className="field-help">{t("grant.keys.help")}</p>
        <label>{t(editing ? "grant.budget.edit" : "grant.budget")}<span className="grant-input"><input type="number" min={0.01} step={0.01} inputMode="decimal" value={form.budget} aria-label={t("grant.budgetAria")} aria-invalid={formError === "budget"} disabled={busy} onChange={event => setForm({ ...form, budget: event.target.value })} />USD</span></label>
        {editing && <p className="field-help">{t("grant.budget.editHelp")}</p>}
        <label>{t("grant.starts.label")}<input type="datetime-local" value={form.startsAt} aria-label={t("grant.startsAria")} aria-invalid={formError === "start" || formError === "range"} disabled={busy} onChange={event => setForm({ ...form, startsAt: event.target.value })} /></label>
        <fieldset className="grant-expiry" disabled={busy}>
          <legend>{t("grant.expiry.legend")}</legend>
          <label className="grant-radio"><input type="radio" name="grant-expiry-mode" checked={estimating} disabled={billingStatuses.length === 0 && !estimating} onChange={() => setForm({ ...form, expiryMode: "billing-estimate" })} /><span>{t("grant.expiry.estimate")}</span></label>
          <label className="grant-radio"><input type="radio" name="grant-expiry-mode" checked={!estimating} onChange={() => setForm({ ...form, expiryMode: "exact", estimateConfirmedFor: null })} /><span>{t("grant.expiry.exact")}</span></label>
        </fieldset>
        {estimating ? <>
          <label>{t("grant.billingAccount")}<select value={form.billingAccountId ?? ""} aria-label={t("grant.billingAccountAria")} disabled={busy} onChange={event => setForm({ ...form, billingAccountId: event.target.value || null, estimateConfirmedFor: null })}>
            <option value="">{t("billing.accountPick")}</option>
            {billingStatuses.map(billing => <option key={billing.accountId} value={billing.accountId}>{accountName(billing.accountId)}</option>)}
          </select></label>
          {estimate.blockedBy === null
            ? <p className="grant-estimate" role="status"><strong>{fullTime(estimate.expiresAt)}</strong> <span className="tag warning">{t("grant.estimateTag")}</span><br />{t("grant.estimate.basis", { zone: timeZoneLabel() })}</p>
            : <p className="gate-reason" role="status">{t(ESTIMATE_BLOCK_KEYS[estimate.blockedBy])}</p>}
          {estimate.blockedBy === null && <label className="grant-confirm"><input type="checkbox" checked={form.estimateConfirmedFor === estimate.expiresAt} onChange={event => setForm({ ...form, estimateConfirmedFor: event.target.checked ? estimate.expiresAt : null })} /><span>{t("grant.estimate.confirm")}</span></label>}
        </> : <label>{t("grant.expires.label")}<input type="datetime-local" value={form.expiresAt} aria-label={t("grant.expiresAria")} aria-invalid={formError === "expiry" || formError === "range" || formError === "past" || formError === "tooLong"} onChange={event => setForm({ ...form, expiresAt: event.target.value })} /></label>}
        <label className="grant-confirm"><input type="checkbox" checked={form.confirmed} disabled={busy} onChange={event => setForm({ ...form, confirmed: event.target.checked })} /><span>{t(estimating ? "grant.confirm.balance" : "grant.confirm")}</span></label>
        {formError && <p className="gate-reason" role="alert">{t(ERROR_KEYS[formError])}</p>}
        <p className="field-help">{t("grant.safety")}</p>
        <div className="grant-actions">
          <button type="submit" className="primary" disabled={busy || keys.length === 0}>{action.pending ? <Busy /> : t(editing ? "grant.save.edit" : "grant.save")}</button>
          {editing && <button type="button" disabled={busy} onClick={() => { setForm(grantFormFrom(null, billingStatuses, snapshot.generatedAt, keyIds)); setFormError(null); }}>{t("grant.form.cancel")}</button>}
        </div>
      </form>
      <h3 id="billing-title" className="grant-subtitle">{t("billing.title")}<Tip tip={t("billing.tip")} label={t("billing.tipLabel")} /></h3>
      {billings.length === 0 ? <p className="field-help">{t("billing.none")}</p> : <ul className="grant-pools" aria-labelledby="billing-title">
        {billings.map(billing => <li key={billing.accountId} className="grant-pool grant-billing-row">
          <span>{t("billing.saved.line", { account: accountName(billing.accountId), cycle: t(billing.cycle === "annual" ? "billing.cycle.annual" : "billing.cycle.monthly"), time: fullTime(billing.nextBillingAt) })}</span>
          <span className="grant-actions">
            <button type="button" disabled={busy} onClick={() => { setEditingBilling(billing.accountId); setBillingForm(billingFormFrom(billing, eligible)); setBillingError(null); }}>{t("grant.edit")}</button>
            <button type="button" disabled={busy} onClick={() => { void action.run(() => update({ apiCreditBillingRemove: billing.accountId }), () => t("billing.cleared")); }}>{t("billing.clear")}</button>
          </span>
        </li>)}
      </ul>}
      <form className="grant-form billing-form" aria-labelledby="billing-title" onSubmit={event => { event.preventDefault(); if (!busy) saveBilling(); }}>
        <p className="field-help">{t("billing.manual")}</p>
        {eligible.length === 0 && <p className="gate-reason" role="status">{t("billing.noAccount")}</p>}
        <label>{t("billing.account")}<select value={billingForm.accountId} aria-label={t("billing.accountAria")} aria-invalid={billingError === "account"} disabled={busy || eligible.length === 0 || editingBilling !== null} onChange={event => setBillingForm({ ...billingForm, accountId: event.target.value })}>
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
        <div className="grant-actions">
          <button type="submit" className="primary" disabled={busy || eligible.length === 0}>{action.pending ? <Busy /> : t("billing.save")}</button>
          {editingBilling !== null && <button type="button" disabled={busy} onClick={() => { setEditingBilling(null); setBillingForm(billingFormFrom(null, freeEligible)); setBillingError(null); }}>{t("grant.form.cancel")}</button>}
        </div>
      </form>
      <div className="rule grant-balance">
        <div className="t">{t("balance.title")} <Tip tip={t("balance.tip")} label={t("balance.tipLabel")} /></div>
        <div className="d">{t("balance.desc")}</div>
        <div className="ctl"><Switch checked={Boolean(policy.balanceApiCredit)} disabled={busy || (!policy.balanceApiCredit && keys.length === 0)} label={t("balance.title")} onChange={balanceApiCredit => { void action.run(() => update({ balanceApiCredit }), () => balanceApiCredit ? t("balance.on") : t("balance.off")); }} /></div>
      </div>
      {!policy.useApiCreditAfterLimit && <p className="field-help">{t("balance.needPaid")}</p>}
      {mode !== "expiry-balanced" && <p className="field-help">{t("balance.needMode")}</p>}
      {policy.balanceApiCredit && grants.length === 0 && <p className="gate-reason" role="status">{t("balance.blocked", { reason: t("balance.block.GRANT_NONE") })}</p>}
      {grantStatuses.some(status => status.monthlyLimitReached) && <p className="gate-reason" role="status">{t("balance.block.MONTHLY_LIMIT")}</p>}
      {keys.length > 0 && <p className="field-help">{t("policy.apiCreditPerKeyCap", { limit: usdLimit(policy.apiCreditMonthlyLimitUsd ?? DEFAULT_API_CREDIT_LIMIT_USD) })} {t("policy.apiCreditUsage", { amount: apiCreditSpend(monthly.usedUsd) })}</p>}
      <ActionFeedback error={action.error} message={action.message} />
    </div>
  </div>;
}
