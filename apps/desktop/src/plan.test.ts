import { test } from "node:test";
import assert from "node:assert/strict";
import { planLabel } from "./plan.ts";

test("Claude plan values show as Max and Ultra, unknown values stay raw", () => {
  assert.equal(planLabel("anthropic", "max"), "Max");
  assert.equal(planLabel("anthropic", "Ultra"), "Ultra");
  assert.equal(planLabel("anthropic", "Max"), "Max");
  assert.equal(planLabel("anthropic", "pro"), "Pro");
  assert.equal(planLabel("anthropic", "mystery_tier"), "mystery_tier");
});

test("Codex prolite is Pro 100 and only confirmed names are rewritten", () => {
  assert.equal(planLabel("openai", "prolite"), "Pro 100");
  assert.equal(planLabel("openai", "pro"), "Pro");
  assert.equal(planLabel("openai", "promax"), "promax");
  assert.equal(planLabel("google", "pro"), "pro");
});
