import { test } from "node:test";
import assert from "node:assert/strict";
import { LANGUAGE_KEY, cacheResolvedLanguage, resolveLocale, type Locale } from "./i18n.ts";

class MemoryStorage {
  readonly items = new Map<string, string>();
  getItem(key: string) { return this.items.get(key) ?? null; }
  setItem(key: string, value: string) { this.items.set(key, value); }
}

/// 앱 시작을 흉내 낸다: 화면 언어를 정하고(i18n.ts), 네이티브 해석 언어를 캐시한 뒤(App.tsx) 필요하면 다시 불러온다.
/// 다시 불러온 횟수를 돌려주고, 수렴하지 않으면 상한에서 멈춘다.
function reloadsUntilStable(storage: MemoryStorage | null, webViewLanguages: string[], nativeResolved: Locale): number {
  let reloads = 0;
  while (reloads < 10) {
    const shown = resolveLocale(storage, webViewLanguages);
    if (!cacheResolvedLanguage(storage, nativeResolved, shown)) return reloads;
    reloads += 1;
  }
  return reloads;
}

test("system language choice converges after one reload when the WebView language differs from the native one", () => {
  // 시스템 선택: 네이티브는 한국어로 해석하지만 WebView는 영어로 보고한다.
  const storage = new MemoryStorage();
  assert.equal(reloadsUntilStable(storage, ["en-US"], "ko"), 1);
  assert.equal(storage.getItem(LANGUAGE_KEY), "ko");
  assert.equal(resolveLocale(storage, ["en-US"]), "ko");
});

test("a matching WebView language never reloads, and later starts stay stable", () => {
  const storage = new MemoryStorage();
  assert.equal(reloadsUntilStable(storage, ["ko-KR"], "ko"), 0);
  assert.equal(reloadsUntilStable(storage, ["en-US"], "ko"), 0);
});

test("a changed native language replaces the cache and reloads once", () => {
  const storage = new MemoryStorage();
  storage.setItem(LANGUAGE_KEY, "ko");
  assert.equal(reloadsUntilStable(storage, ["ko-KR"], "id"), 1);
  assert.equal(storage.getItem(LANGUAGE_KEY), "id");
});

test("without usable storage the screen never reloads, since a reload could not change the language", () => {
  assert.equal(cacheResolvedLanguage(null, "ko", "en"), false);
  const blocked = { setItem() { throw new Error("blocked"); } };
  assert.equal(cacheResolvedLanguage(blocked, "ko", "en"), false);
  assert.equal(resolveLocale({ getItem() { throw new Error("blocked"); } }, ["id-ID"]), "id");
  assert.equal(resolveLocale(null, []), "en");
});

test("an unknown cached value is ignored in favor of the WebView language", () => {
  const storage = new MemoryStorage();
  storage.setItem(LANGUAGE_KEY, "system");
  assert.equal(resolveLocale(storage, ["ko-KR"]), "ko");
});
