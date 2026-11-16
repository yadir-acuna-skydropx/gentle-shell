#!/usr/bin/env node
// gentle-partitioner installer — seed RDD review-lens model routing (S37, T40).
//
// Why: the Pi host-relay native review routes each review lens
// (review-risk/resilience/readability/reliability) through the global
// gentle-ai models.json. Those four lens keys are FAIL-CLOSED: unlike
// review-refuter/review-validator they get no default, so when the key is
// absent from models.json the review refuses to launch with
// "no model is configured for review-risk ... the review-risk entry is missing".
// The global models.json is otherwise written only by the interactive
// "/gentle:profiles -> Make default" apply; there is no CLI seam. On a fresh
// server install that leaves RDD review permanently broken.
//
// This helper writes the four lens keys into that models.json, IDEMPOTENTLY:
// it only adds a key that is absent and never overwrites an entry the user
// already set, never rewrites an unparseable file, and never touches other
// agents. Entry schema matches lib/model-routing-authority.ts: a per-agent
// value is a model-id string or { model, thinking }. Default: sonnet + medium
// (a reasoning model with thinking is the right tool for review, independent
// from the heavier implementation model).
//
// Caveat: this seeds only the GLOBAL layer. A session pinned to an explicit
// profile reads that profile instead, so a pinned session still needs the lens
// keys in its profile.

import { existsSync, readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, dirname } from "node:path";

export const REVIEW_LENS_AGENTS = [
  "review-risk",
  "review-resilience",
  "review-readability",
  "review-reliability",
];

const THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const SAFE_MODEL_ID_PATTERN = /^[A-Za-z0-9._~:@/+%-]+$/;

// Resolve the gentle-ai config home the relay reads, matching
// resolveGentlePiConfigHome's common cases: an explicit override wins, then the
// bundled prefix default ~/.gentle-shell/config/gentle-ai. `env`/`home` are
// injectable for tests.
export function resolveConfigHome(env = process.env, home = homedir()) {
  const override = env.SEED_REVIEW_MODELS_CONFIG_HOME || env.GENTLE_PI_CONFIG_HOME;
  if (override && override.trim()) return override.trim();
  return join(home, ".gentle-shell", "config", "gentle-ai");
}

// Merge the review-lens keys into an existing models.json object WITHOUT
// clobbering: returns { config, added, kept }. A key already present (any
// shape) is left exactly as-is. `existing` must be a plain object (the parsed
// models.json) or undefined/null for "no file".
export function mergeReviewModels(existing, { model, thinking } = {}) {
  const isRecord = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
  const base = isRecord(existing) ? existing : {};
  const useModel = typeof model === "string" && SAFE_MODEL_ID_PATTERN.test(model) ? model : "sonnet";
  const useThinking = THINKING_LEVELS.includes(thinking) ? thinking : "medium";
  const config = { ...base };
  const added = [];
  const kept = [];
  for (const agent of REVIEW_LENS_AGENTS) {
    if (Object.prototype.hasOwnProperty.call(base, agent)) {
      kept.push(agent);
      continue;
    }
    config[agent] = { model: useModel, thinking: useThinking };
    added.push(agent);
  }
  return { config, added, kept };
}

function parseArgs(argv) {
  const args = { dryRun: false, model: "sonnet", thinking: "medium" };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--dry-run") args.dryRun = true;
    else if (a === "--model") args.model = argv[++i];
    else if (a === "--thinking" || a === "--effort") args.thinking = argv[++i];
    else if (a === "--help" || a === "-h") args.help = true;
  }
  return args;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(
      "seed-review-models.mjs — ensure the four RDD review-lens keys exist in the\n" +
        "global gentle-ai models.json (idempotent; never overwrites user entries).\n" +
        "  --model <id>       default sonnet\n" +
        "  --thinking <level> default medium (off|minimal|low|medium|high|xhigh|max)\n" +
        "  --dry-run          print the plan without writing\n",
    );
    return 0;
  }
  const path = join(resolveConfigHome(), "models.json");

  let existing;
  if (existsSync(path)) {
    let raw;
    try {
      raw = JSON.parse(readFileSync(path, "utf8"));
    } catch {
      process.stderr.write(`review models: ${path} is not valid JSON — left untouched\n`);
      return 1;
    }
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
      process.stderr.write(`review models: ${path} is not a JSON object — left untouched\n`);
      return 1;
    }
    existing = raw;
  }

  const { config, added, kept } = mergeReviewModels(existing, { model: args.model, thinking: args.thinking });

  if (added.length === 0) {
    process.stdout.write(`review models: all lenses already routed in ${path} (${kept.join(", ")})\n`);
    return 0;
  }

  const plan = `review models: add ${added.join(", ")} = {model:${args.model}, thinking:${args.thinking}} to ${path}` +
    (kept.length ? ` (kept ${kept.join(", ")})` : "");
  if (args.dryRun) {
    process.stdout.write(`would ${plan}\n`);
    return 0;
  }
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(config, null, 2)}\n`, "utf8");
  process.stdout.write(`${plan}\n`);
  return 0;
}

// Run only when executed directly, not when imported by tests.
if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main();
}
