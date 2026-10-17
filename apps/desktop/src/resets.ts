import type { ApiError } from "./types";

export interface ResetCredit {
  id: string; program: string; remainingCount: number; usable: boolean;
  expiresAt: number | null; startsAt: number | null; clears: string[]; requiresLimit: boolean; status: string;
}
export interface ResetOperation {
  requestId: string; accountId: string; creditId: string;
  state: "preparing" | "submitted" | "unknown" | "succeeded" | "declined" | "resolved";
  code: string; startedAt: number; updatedAt: number;
}
export interface ResetInventory {
  accountId: string; state: "known" | "unknown" | "unsupported";
  availableCount: number | null; redeemableCount: number | null; credits: ResetCredit[];
  observedAt: number; quoteId: string | null; operation: ResetOperation | null;
}
const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const id = (value: unknown): value is string => typeof value === "string" && /^[a-zA-Z0-9_-]{1,256}$/.test(value);
const date = (value: unknown) => value === null || count(value);
function invalid(): never { throw { code: "PROTOCOL_MISMATCH", message: "Invalid reset-credit response", retryable: false } satisfies ApiError; }
export function parseResetOperation(raw: unknown): ResetOperation {
  if (!object(raw) || !id(raw.requestId) || !id(raw.accountId) || !id(raw.creditId)
    || !["preparing", "submitted", "unknown", "succeeded", "declined", "resolved"].includes(String(raw.state))
    || !id(raw.code) || !count(raw.startedAt) || !count(raw.updatedAt)) invalid();
  return raw as unknown as ResetOperation;
}
export function parseResetInventory(raw: unknown): ResetInventory {
  if (!object(raw) || !id(raw.accountId) || !["known", "unknown", "unsupported"].includes(String(raw.state))
    || !count(raw.observedAt) || !(raw.quoteId === null || id(raw.quoteId))
    || !(raw.availableCount === null || count(raw.availableCount)) || !(raw.redeemableCount === null || count(raw.redeemableCount))
    || (raw.state === "known" && (!count(raw.availableCount) || !count(raw.redeemableCount)))
    || !Array.isArray(raw.credits) || raw.credits.length > 256) invalid();
  for (const credit of raw.credits) {
    if (!object(credit) || !id(credit.id) || !id(credit.program) || !count(credit.remainingCount)
      || typeof credit.usable !== "boolean" || typeof credit.requiresLimit !== "boolean" || !id(credit.status)
      || !date(credit.expiresAt) || !date(credit.startsAt) || !Array.isArray(credit.clears)
      || credit.clears.length > 64 || !credit.clears.every(scope => typeof scope === "string" && /^[a-zA-Z0-9:_-]{1,128}$/.test(scope))) invalid();
  }
  if (raw.operation !== null) parseResetOperation(raw.operation);
  return raw as unknown as ResetInventory;
}
export function resetBlocked(operation: ResetOperation | null): boolean {
  return operation !== null && ["preparing", "submitted", "unknown"].includes(operation.state);
}
export function canUseReset(inventory: ResetInventory, credit: ResetCredit, now: number): boolean {
  return inventory.state === "known" && inventory.quoteId !== null && !resetBlocked(inventory.operation)
    && now >= inventory.observedAt && now - inventory.observedAt <= 120_000
    && credit.usable && credit.remainingCount > 0 && inventory.credits.includes(credit)
    && (credit.expiresAt === null || credit.expiresAt > now) && (credit.startsAt === null || credit.startsAt <= now);
}
