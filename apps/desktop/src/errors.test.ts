import { test } from "node:test";
import assert from "node:assert/strict";
import { dictionaries, errorCodeText, locale } from "./i18n.ts";
import { describeError } from "./errors.ts";

const LOCALES = ["en", "ko", "id"] as const;
const codeKeys = Object.keys(dictionaries.en).filter(key => key.startsWith("error.code."));
const placeholders = (text: unknown) => (typeof text === "string" ? [...text.matchAll(/\{(\w+)\}/g)].map(match => match[1]).sort() : null);

test("every error.code key exists in en, ko and id with the same placeholders", () => {
  for (const key of codeKeys) {
    const reference = placeholders((dictionaries.en as Record<string, unknown>)[key]);
    for (const name of LOCALES) {
      const text = (dictionaries[name] as Record<string, unknown>)[key];
      assert.equal(typeof text, "string", `${name} is missing or non-string: ${key}`);
      assert.notEqual((text as string).trim(), "", `${name} is empty: ${key}`);
      assert.deepEqual(placeholders(text), reference, `${name} placeholders differ: ${key}`);
    }
  }
  for (const name of LOCALES) {
    const keys = Object.keys(dictionaries[name]).filter(key => key.startsWith("error."));
    assert.deepEqual(keys.sort(), Object.keys(dictionaries.en).filter(key => key.startsWith("error.")).sort(), `${name} error.* keys differ`);
  }
});


test("a known code shows the localized sentence and moves the raw message to details", () => {
  const raw = "Claude 공식 상태가 구독 로그인이 아니에요. API 키나 다른 공급자로 자동 바꾸지 않아요.";
  const view = describeError({ code: "AUTH_OVERRIDE_CONFLICT", message: raw, retryable: false });
  assert.equal(view.text, errorCodeText("AUTH_OVERRIDE_CONFLICT"));
  assert.equal(view.text, (dictionaries[locale] as Record<string, string>)["error.code.AUTH_OVERRIDE_CONFLICT"]);
  assert.equal(view.details, raw);
});

test("raw details keep the backend text byte for byte, including names and paths", () => {
  const raw = "설정 파일 /Users/me/.claude/settings.json의 apiKeyHelper 항목이 선택한 구독 인증을 덮어쓸 수 있습니다.";
  const view = describeError({ code: "AUTH_OVERRIDE_CONFLICT", message: raw, retryable: false });
  assert.equal(view.details, raw);
  assert.ok(!view.text.includes("/Users/me"), "the main sentence must not carry the path");
});

test("an unknown code keeps the backend message as the main text with no details", () => {
  const view = describeError({ code: "SOMETHING_NEW", message: "원문 그대로", retryable: false });
  assert.deepEqual(view, { text: "원문 그대로", details: null });
  assert.equal(errorCodeText("SOMETHING_NEW"), null);
  assert.equal(errorCodeText("REQUEST_FAILED"), null);
});

test("details are omitted when the message is empty or identical to the localized text", () => {
  const localized = errorCodeText("SESSION_NOT_FOUND");
  assert.ok(localized);
  assert.equal(describeError({ code: "SESSION_NOT_FOUND", message: "", retryable: false }).details, null);
  assert.equal(describeError({ code: "SESSION_NOT_FOUND", message: "  ", retryable: false }).details, null);
  assert.equal(describeError({ code: "SESSION_NOT_FOUND", message: localized, retryable: false }).details, null);
});

test("codes with the same meaning in two crates share one message", () => {
  for (const [a, b] of [["SHIM_RECURSION", "RECURSIVE_BINARY"], ["TOOL_UNSUPPORTED", "UNSUPPORTED_TOOL"], ["PROJECT_INVALID", "INVALID_PROJECT"], ["HOME_MISSING", "HOME_UNAVAILABLE"]]) {
    for (const name of LOCALES) {
      assert.equal((dictionaries[name] as Record<string, string>)[`error.code.${a}`], (dictionaries[name] as Record<string, string>)[`error.code.${b}`]);
    }
  }
});
