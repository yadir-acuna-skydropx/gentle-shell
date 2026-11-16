// Unit tests for seed-review-models.mjs (S37, T40). Pure functions only; no
// filesystem writes, no real config home. Run: node --test seed-review-models.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { REVIEW_LENS_AGENTS, mergeReviewModels, resolveConfigHome } from "./seed-review-models.mjs";

test("seeds all four lens keys when models.json is absent", () => {
  const { config, added, kept } = mergeReviewModels(undefined, { model: "sonnet", thinking: "medium" });
  assert.deepEqual(added.sort(), [...REVIEW_LENS_AGENTS].sort());
  assert.deepEqual(kept, []);
  for (const a of REVIEW_LENS_AGENTS) {
    assert.deepEqual(config[a], { model: "sonnet", thinking: "medium" });
  }
});

test("never overwrites an existing user entry", () => {
  const existing = { "review-risk": { model: "opus", thinking: "high" }, "other-agent": "haiku" };
  const { config, added, kept } = mergeReviewModels(existing, { model: "sonnet", thinking: "medium" });
  assert.ok(kept.includes("review-risk"));
  assert.ok(!added.includes("review-risk"));
  assert.deepEqual(config["review-risk"], { model: "opus", thinking: "high" });
  assert.equal(config["other-agent"], "haiku"); // unrelated agents untouched
  assert.ok(added.includes("review-readability"));
});

test("is idempotent: a second merge adds nothing", () => {
  const first = mergeReviewModels(undefined, {}).config;
  const { added } = mergeReviewModels(first, {});
  assert.deepEqual(added, []);
});

test("falls back to sonnet/medium on invalid model or thinking", () => {
  const { config } = mergeReviewModels(undefined, { model: "bad id with spaces", thinking: "turbo" });
  assert.deepEqual(config["review-risk"], { model: "sonnet", thinking: "medium" });
});

test("accepts a valid custom model and thinking", () => {
  const { config } = mergeReviewModels(undefined, { model: "haiku", thinking: "low" });
  assert.deepEqual(config["review-readability"], { model: "haiku", thinking: "low" });
});

test("resolveConfigHome honors overrides then falls back to the bundled prefix", () => {
  assert.equal(
    resolveConfigHome({ SEED_REVIEW_MODELS_CONFIG_HOME: "/tmp/cfg" }, "/home/u"),
    "/tmp/cfg",
  );
  assert.equal(
    resolveConfigHome({ GENTLE_PI_CONFIG_HOME: "/tmp/pi" }, "/home/u"),
    "/tmp/pi",
  );
  assert.equal(
    resolveConfigHome({}, "/home/u"),
    join("/home/u", ".gentle-shell", "config", "gentle-ai"),
  );
});
