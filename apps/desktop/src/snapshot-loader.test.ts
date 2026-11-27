import assert from "node:assert/strict";
import test from "node:test";
import { snapshotLoader } from "./snapshot-loader.ts";

/// 서비스 대신 쓰는 읽기. 서비스처럼 요청을 받은 시점의 계정 목록으로 답하고, 응답은 테스트가 직접 풀어 준다.
function fixture() {
  let accounts = ["justice", "lio"];
  const shown: string[][] = [];
  const pending: (() => void)[] = [];
  const read = (): Promise<void> => {
    const { promise, resolve } = Promise.withResolvers<void>();
    const seen = [...accounts];
    pending.push(() => { shown.push(seen); resolve(); });
    return promise;
  };
  return { shown, pending, read, remove(name: string) { accounts = accounts.filter(item => item !== name); } };
}
const settle = () => new Promise(resolve => setImmediate(resolve));

test("a reload after removing an account does not reuse a poll that started before the removal", async () => {
  const service = fixture();
  const { load } = snapshotLoader(service.read, () => true);
  const poll = load();
  await settle();
  service.remove("justice");
  const afterRemove = load(false, true);
  service.pending.shift()!();
  await poll;
  assert.deepEqual(service.shown.at(-1), ["justice", "lio"], "the poll read before the removal");
  await settle();
  service.pending.shift()!();
  await afterRemove;
  assert.deepEqual(service.shown.at(-1), ["lio"], "the reload awaited by the success message reads after the removal");
});

test("a plain poll still shares a read already in flight", async () => {
  const service = fixture();
  const { load } = snapshotLoader(service.read, () => true);
  const first = load();
  const second = load();
  await settle();
  assert.equal(service.pending.length, 1);
  service.pending.shift()!();
  await Promise.all([first, second]);
});
