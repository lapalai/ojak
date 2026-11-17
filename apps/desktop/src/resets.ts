import type { ApiError } from "./types";

export type ResetType = "session" | "weekly" | "full" | "fable" | "provider" | "unknown";
export type ResetState = "known" | "unknown" | "unsupported" | "restricted";
export interface ResetCredit {
  id: string; program: string; remainingCount: number; usable: boolean;
  expiresAt: number | null; startsAt: number | null; clears: string[]; requiresLimit: boolean; status: string;
  /// Provider-neutral category. `unknown` never authorizes a spend, whatever else the offer claims.
  resetType: ResetType;
  /// Fable-style partial reimbursement: gives back weekly usage of one model only; other weekly usage stays.
  givesBackWeekly: boolean;
  blocking: string[];
  /// Fixed sanitized reason token from the service, never a provider message.
  unavailableReason: string | null;
  cooldownUntil: number | null;
}
export interface ResetOperation {
  requestId: string; accountId: string; creditId: string;
  state: "preparing" | "submitted" | "unknown" | "succeeded" | "declined" | "resolved";
  code: string; startedAt: number; updatedAt: number;
}
export type ResetSource = "oauth" | "web";
export type ResetWebState = "disconnected" | "waiting_browser" | "login_required" | "syncing" | "ready" | "error";
/// Connection state of the account-bound Claude web read. Times are epoch milliseconds; `errorCode` is a fixed service token.
export interface ResetWebStatus {
  accountId: string; connected: boolean; state: ResetWebState;
  lastAttemptAt: number | null; lastSuccessAt: number | null; nextRefreshAt: number | null; errorCode: string | null;
}
export interface ResetInventory {
  accountId: string; state: ResetState;
  availableCount: number | null; redeemableCount: number | null; credits: ResetCredit[];
  observedAt: number; quoteId: string | null; operation: ResetOperation | null;
  /// Why the source restricted or failed the lookup (fixed token). Only meaningful when state is not `known`.
  lookupReason: string | null;
  /// Where the offers were read. A `web` inventory is read-only in Ojak: it never carries a quote or a use path.
  source: ResetSource;
  /// Web connection status. Null for an account that was never connected to the web read.
  web: ResetWebStatus | null;
}
const WEB_STATES: readonly ResetWebState[] = ["disconnected", "waiting_browser", "login_required", "syncing", "ready", "error"];
/// Match the service's five-minute observation lifetime; a missed refresh is not a current balance.
export const WEB_STALE_MS = 5 * 60_000;
const RESET_TYPES: readonly ResetType[] = ["session", "weekly", "full", "fable", "provider", "unknown"];
const STATES: readonly string[] = ["known", "unknown", "unsupported", "restricted"];

/// Scope identifiers this app can describe and show in its confirmation. Anything else is shown as an
/// escaped identifier and can never be confirmed. Keys are `reset.scope.<value>` suffixes in the dictionaries.
const SCOPES: Record<string, string> = {
  "anthropic:5h": "session",
  "anthropic:7d": "weekly",
  "anthropic:7d:opus": "opus",
  "anthropic:7d:sonnet": "sonnet",
  "anthropic:7d:fable": "fable",
  "anthropic:reset:seven_day_cowork": "cowork",
  "anthropic:reset:seven_day_omelette": "omelette",
  "anthropic:reset:seven_day_oauth_apps": "oauthApps",
  "openai-codex:rate_limits": "codex",
};
/// Dictionary suffix for a recognized scope, or null for an identifier this app does not know.
export function resetScopeKey(scope: string): string | null {
  return Object.prototype.hasOwnProperty.call(SCOPES, scope) ? SCOPES[scope] : null;
}

const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const id = (value: unknown): value is string => typeof value === "string" && /^[a-zA-Z0-9_-]{1,256}$/.test(value);
const webCode = (value: unknown): value is string => typeof value === "string" && /^[A-Z0-9_]{1,64}$/.test(value);
const reason = (value: unknown): value is string => typeof value === "string" && /^[a-zA-Z0-9_.-]{1,64}$/.test(value);
const scope = (value: unknown): value is string => typeof value === "string" && /^[a-zA-Z0-9:_-]{1,128}$/.test(value);
const date = (value: unknown) => value === null || count(value);
function invalid(): never { throw { code: "PROTOCOL_MISMATCH", message: "Invalid reset-credit response", retryable: false } satisfies ApiError; }
export function parseResetOperation(raw: unknown): ResetOperation {
  if (!object(raw) || !id(raw.requestId) || !id(raw.accountId) || !id(raw.creditId)
    || !["preparing", "submitted", "unknown", "succeeded", "declined", "resolved"].includes(String(raw.state))
    || !id(raw.code) || !count(raw.startedAt) || !count(raw.updatedAt)) invalid();
  return raw as unknown as ResetOperation;
}
/// A service that does not report the web read (an older one) is rejected, never read as "not connected".
export function parseResetWebStatus(raw: unknown): ResetWebStatus {
  if (!object(raw) || !id(raw.accountId) || typeof raw.connected !== "boolean"
    || !WEB_STATES.includes(raw.state as ResetWebState)
    // Connected and disconnected must agree: a contradiction is not trusted either way.
    || (raw.state === "disconnected") === raw.connected
    || !date(raw.lastAttemptAt) || !date(raw.lastSuccessAt) || !date(raw.nextRefreshAt)
    || !(raw.errorCode === null || webCode(raw.errorCode))) invalid();
  return {
    accountId: raw.accountId, connected: raw.connected, state: raw.state as ResetWebState,
    lastAttemptAt: raw.lastAttemptAt as number | null, lastSuccessAt: raw.lastSuccessAt as number | null,
    nextRefreshAt: raw.nextRefreshAt as number | null, errorCode: raw.errorCode as string | null,
  };
}
function parseCredit(credit: unknown): ResetCredit {
  if (!object(credit) || !id(credit.id) || !id(credit.program) || !count(credit.remainingCount)
    || typeof credit.usable !== "boolean" || typeof credit.requiresLimit !== "boolean" || !id(credit.status)
    || !date(credit.expiresAt) || !date(credit.startsAt) || !Array.isArray(credit.clears)
    || credit.clears.length > 64 || !credit.clears.every(scope)) invalid();
  // A service that cannot distinguish restricted reads from zero must be updated, not trusted as a zero balance.
  const { resetType, givesBackWeekly, blocking, unavailableReason, cooldownUntil } = credit;
  if (typeof givesBackWeekly !== "boolean"
    || !Array.isArray(blocking) || blocking.length > 64 || !blocking.every(scope)
    || !(unavailableReason === null || reason(unavailableReason))
    || !date(cooldownUntil) || !reason(resetType)) invalid();
  return {
    id: credit.id, program: credit.program, remainingCount: credit.remainingCount, usable: credit.usable,
    expiresAt: credit.expiresAt as number | null, startsAt: credit.startsAt as number | null,
    clears: credit.clears as string[], requiresLimit: credit.requiresLimit, status: credit.status,
    resetType: RESET_TYPES.includes(resetType as ResetType) ? resetType as ResetType : "unknown",
    givesBackWeekly,
    blocking: blocking as string[],
    unavailableReason: unavailableReason as string | null,
    cooldownUntil: cooldownUntil as number | null,
  };
}
export function parseResetInventory(raw: unknown): ResetInventory {
  if (!object(raw) || !id(raw.accountId) || !STATES.includes(String(raw.state))
    || !count(raw.observedAt) || !(raw.quoteId === null || id(raw.quoteId))
    || !(raw.availableCount === null || count(raw.availableCount)) || !(raw.redeemableCount === null || count(raw.redeemableCount))
    || (raw.state === "known" && (!count(raw.availableCount) || !count(raw.redeemableCount)))
    // A restricted lookup says nothing about the balance: no totals, never a zero.
    || (raw.state === "restricted" && (raw.availableCount !== null || raw.redeemableCount !== null))
    || !(raw.lookupReason === null || reason(raw.lookupReason))
    || (raw.source !== "oauth" && raw.source !== "web") || !("web" in raw)
    || !Array.isArray(raw.credits) || raw.credits.length > 256) invalid();
  const credits = raw.credits.map(parseCredit);
  const operation = raw.operation === null ? null : parseResetOperation(raw.operation);
  const web = raw.web === null ? null : parseResetWebStatus(raw.web);
  // A web observation without a live connection, or a status for another account, cannot be attributed.
  if (web !== null && web.accountId !== raw.accountId) invalid();
  if (raw.source === "web" && (web === null || !web.connected)) invalid();
  const state = raw.state as ResetState;
  const source = raw.source as ResetSource;
  return {
    accountId: raw.accountId, state, observedAt: raw.observedAt,
    availableCount: raw.availableCount as number | null, redeemableCount: raw.redeemableCount as number | null,
    credits, operation, lookupReason: raw.lookupReason as string | null, source, web,
    // Only a fully known lookup can carry spend authority, whatever a stale record still says. A web observation never does.
    quoteId: state === "known" && source === "oauth" ? raw.quoteId as string | null : null,
  };
}
/// True when the connected web read has no current observation: not connected, waiting for the app or a login,
/// failing, never read, or older than one refresh interval. Such offers are only the last thing seen.
export function resetWebStale(inventory: ResetInventory, now: number): boolean {
  const web = inventory.web;
  if (inventory.source !== "web" || web === null || !web.connected || (web.state !== "ready" && web.state !== "syncing")) return true;
  if (inventory.state !== "known" || web.lastSuccessAt === null || now < web.lastSuccessAt) return true;
  return now - web.lastSuccessAt > WEB_STALE_MS;
}
export type ResetWebOffer = "available" | "unavailable" | "stale";
/// What a web-read offer means to a person. This is information for the official page only; Ojak never uses it.
export function resetWebOffer(inventory: ResetInventory, credit: ResetCredit, now: number): ResetWebOffer {
  if (resetWebStale(inventory, now)) return "stale";
  const ready = credit.usable && credit.unavailableReason === null && credit.blocking.length === 0 && credit.remainingCount > 0
    && (credit.expiresAt === null || credit.expiresAt > now) && (credit.startsAt === null || credit.startsAt <= now)
    && (credit.cooldownUntil === null || credit.cooldownUntil <= now);
  return ready ? "available" : "unavailable";
}
export function resetBlocked(operation: ResetOperation | null): boolean {
  return operation !== null && ["preparing", "submitted", "unknown"].includes(operation.state);
}
/// The scope or type is something this app cannot describe, so the user could not give an informed confirmation.
function unrecognized(credit: ResetCredit): boolean {
  return credit.resetType === "unknown" || credit.clears.length === 0 || credit.clears.some(item => resetScopeKey(item) === null)
    // A Fable-style reimbursement and the weekly give-back flag must agree; a mismatch is not described correctly.
    || (credit.resetType === "fable") !== credit.givesBackWeekly;
}
/// Why this offer cannot be confirmed right now, or null when it can. Tokens are fixed and map to UI text.
export type ResetUseBlock = "web" | "state" | "quote" | "operation" | "stale" | "foreign" | "unusable" | "count" | "reason"
  | "expired" | "notStarted" | "cooldown" | "unrecognized";
export function resetUseBlock(inventory: ResetInventory, credit: ResetCredit, now: number): ResetUseBlock | null {
  // A web read is for the official page only; Ojak has no path that could spend from it.
  if (inventory.source !== "oauth") return "web";
  if (inventory.state !== "known") return "state";
  if (inventory.quoteId === null) return "quote";
  if (resetBlocked(inventory.operation)) return "operation";
  if (now < inventory.observedAt || now - inventory.observedAt > 120_000) return "stale";
  if (!inventory.credits.includes(credit)) return "foreign";
  if (unrecognized(credit)) return "unrecognized";
  if (!credit.usable) return "unusable";
  if (credit.unavailableReason !== null || credit.blocking.length > 0) return "reason";
  if (credit.remainingCount <= 0) return "count";
  if (credit.expiresAt !== null && credit.expiresAt <= now) return "expired";
  if (credit.startsAt !== null && credit.startsAt > now) return "notStarted";
  if (credit.cooldownUntil !== null && credit.cooldownUntil > now) return "cooldown";
  return null;
}
export function canUseReset(inventory: ResetInventory, credit: ResetCredit, now: number): boolean {
  return resetUseBlock(inventory, credit, now) === null;
}
