import { test } from "node:test";
import assert from "node:assert/strict";
import { createPromptPredictor, fromEnv, type PromptPredictor, type PredictResponse } from "../lib/prompt-prediction.ts";
import { GentlePromptEditor } from "../extensions/gentle-shell.ts";
import { stripAnsi } from "../lib/terminal-theme.ts";
import { visibleWidth } from "@earendil-works/pi-tui";

// A controllable promise so a test can resolve or reject each fetch at will.
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (error: unknown) => void } {
	let resolve!: (value: T) => void;
	let reject!: (error: unknown) => void;
	const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
	return { promise, resolve, reject };
}

interface FetchCall {
	url: string;
	init: { method: string; headers: Record<string, string>; body: string; signal?: AbortSignal };
	settle(value: PredictResponse): void;
	fail(error: unknown): void;
}

// Records every fetch and hands the test control over each response, rejecting a
// pending call when its AbortSignal fires (mirroring a real aborted fetch).
function makeFetch() {
	const calls: FetchCall[] = [];
	const fetch = (url: string, init: FetchCall["init"]): Promise<PredictResponse> => {
		const d = deferred<PredictResponse>();
		init.signal?.addEventListener("abort", () => d.reject(new Error("aborted")));
		calls.push({ url, init, settle: d.resolve, fail: d.reject });
		return d.promise;
	};
	return { fetch, calls };
}

// Deterministic timers: run the scheduled callbacks for a given delay on demand.
function makeTimers() {
	let seq = 0;
	const timers = new Map<number, { fn: () => void; ms: number }>();
	return {
		setTimeout: (fn: () => void, ms: number): unknown => { const id = ++seq; timers.set(id, { fn, ms }); return id; },
		clearTimeout: (handle: unknown): void => { timers.delete(handle as number); },
		runByMs(ms: number): void {
			for (const [id, t] of [...timers]) if (t.ms === ms) { timers.delete(id); t.fn(); }
		},
		pending(ms: number): number {
			let n = 0;
			for (const t of timers.values()) if (t.ms === ms) n++;
			return n;
		},
	};
}

const flush = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));
const ok = (suggestion: unknown): PredictResponse => ({ ok: true, status: 200, json: async () => ({ suggestion }) });

const DEBOUNCE = 10;
const TIMEOUT = 50;
function harness() {
	const { fetch, calls } = makeFetch();
	const timers = makeTimers();
	const predictor = createPromptPredictor({
		url: "http://127.0.0.1:9", token: "tok", fetch,
		setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout,
		debounceMs: DEBOUNCE, timeoutMs: TIMEOUT,
	});
	return { predictor, calls, timers };
}

test("debounce collapses a burst into one fetch for the last text", () => {
	const { predictor, calls, timers } = harness();
	predictor.request("he", () => {});
	predictor.request("hel", () => {});
	predictor.request("hello", () => {});
	assert.equal(calls.length, 0, "nothing fetched before the debounce elapses");
	timers.runByMs(DEBOUNCE);
	assert.equal(calls.length, 1, "exactly one fetch after the burst settles");
	assert.equal(calls[0]!.init.body, JSON.stringify({ text: "hello", mode: "chat" }));
});

test("the predict request carries the exact auth header and JSON body", () => {
	const { predictor, calls, timers } = harness();
	predictor.request("hola", () => {});
	timers.runByMs(DEBOUNCE);
	assert.equal(calls[0]!.url, "http://127.0.0.1:9/predict");
	assert.equal(calls[0]!.init.method, "POST");
	assert.equal(calls[0]!.init.headers.Authorization, "Bearer tok");
	assert.equal(calls[0]!.init.headers["Content-Type"], "application/json");
	assert.equal(calls[0]!.init.body, JSON.stringify({ text: "hola", mode: "chat" }));
});

test("a non-empty suggestion for the latest text reaches onSuggestion", async () => {
	const { predictor, calls, timers } = harness();
	let got: string | undefined;
	predictor.request("hello", (suffix) => { got = suffix; });
	timers.runByMs(DEBOUNCE);
	calls[0]!.settle(ok(" world"));
	await flush();
	assert.equal(got, " world");
});

test("a newer request aborts the previous in-flight one and ignores its stale response", async () => {
	const { predictor, calls, timers } = harness();
	const seen: string[] = [];
	predictor.request("foo", (s) => seen.push(`foo:${s}`));
	timers.runByMs(DEBOUNCE);
	assert.equal(calls.length, 1);
	predictor.request("foobar", (s) => seen.push(`foobar:${s}`));
	timers.runByMs(DEBOUNCE);
	assert.equal(calls.length, 2);
	assert.equal(calls[0]!.init.signal!.aborted, true, "the superseded request is aborted");
	// Even if the stale response somehow resolves, it must be ignored.
	calls[0]!.settle(ok(" stale"));
	calls[1]!.settle(ok("bar"));
	await flush();
	assert.deepEqual(seen, ["foobar:bar"]);
});

test("a timeout yields no suggestion and never throws", async () => {
	const { predictor, calls, timers } = harness();
	let got: string | undefined;
	predictor.request("hello", (s) => { got = s; });
	timers.runByMs(DEBOUNCE);
	timers.runByMs(TIMEOUT); // abort via timeout
	await flush();
	assert.equal(got, undefined);
});

test("a 500 response yields no suggestion", async () => {
	const { predictor, calls, timers } = harness();
	let got: string | undefined;
	predictor.request("hello", (s) => { got = s; });
	timers.runByMs(DEBOUNCE);
	calls[0]!.settle({ ok: false, status: 500, json: async () => ({}) });
	await flush();
	assert.equal(got, undefined);
});

test("a network error yields no suggestion and never throws", async () => {
	const { predictor, calls, timers } = harness();
	let got: string | undefined;
	predictor.request("hello", (s) => { got = s; });
	timers.runByMs(DEBOUNCE);
	calls[0]!.fail(new Error("network down"));
	await flush();
	assert.equal(got, undefined);
});

test("an empty suggestion is treated as no suggestion", async () => {
	const { predictor, calls, timers } = harness();
	let called = false;
	predictor.request("hello", () => { called = true; });
	timers.runByMs(DEBOUNCE);
	calls[0]!.settle(ok(""));
	await flush();
	assert.equal(called, false);
});

test("learn posts the text to /learn fire-and-forget", () => {
	const { predictor, calls } = harness();
	predictor.learn("remember this");
	assert.equal(calls.length, 1);
	assert.equal(calls[0]!.url, "http://127.0.0.1:9/learn");
	assert.equal(calls[0]!.init.method, "POST");
	assert.equal(calls[0]!.init.headers.Authorization, "Bearer tok");
	assert.equal(calls[0]!.init.body, JSON.stringify({ text: "remember this", mode: "chat" }));
});

test("dispose cancels a pending debounce so no fetch fires", () => {
	const { predictor, calls, timers } = harness();
	predictor.request("hello", () => {});
	predictor.dispose();
	timers.runByMs(DEBOUNCE);
	assert.equal(calls.length, 0);
});

test("fromEnv returns null without the URL or the token", () => {
	assert.equal(fromEnv({}), null);
	assert.equal(fromEnv({ GENTLE_PREDICT_URL: "http://127.0.0.1:9" }), null);
	assert.equal(fromEnv({ GENTLE_PREDICT_TOKEN: "tok" }), null);
});

test("fromEnv builds a predictor when both the URL and token are present", () => {
	const { fetch, calls } = makeFetch();
	const timers = makeTimers();
	const predictor = fromEnv(
		{ GENTLE_PREDICT_URL: "http://127.0.0.1:9", GENTLE_PREDICT_TOKEN: "tok" },
		{ fetch, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout, debounceMs: DEBOUNCE, timeoutMs: TIMEOUT },
	);
	assert.ok(predictor, "a predictor is returned");
	predictor!.learn("hi");
	assert.equal(calls[0]!.url, "http://127.0.0.1:9/learn");
});

// --- Editor-level behavior (S9 ghost text in the shared prompt editor) ---

const fakeTui = { terminal: { rows: 40, columns: 120 }, requestRender() {} };
const editorTheme = { borderColor: (text: string) => text, selectList: {} };
const fakeKeybindings = { matches: () => false };

// A synchronous stub predictor: `request` answers immediately from `suffixFor`,
// and every learned prompt is recorded, so the editor's wiring is exercised
// without the HTTP client (that client is covered by the tests above).
function stubPredictor(suffixFor: (text: string) => string) {
	const learned: string[] = [];
	let disposed = 0;
	const predictor: PromptPredictor = {
		request(text, onSuggestion) { const suffix = suffixFor(text); if (suffix) onSuggestion(suffix); },
		learn(text) { learned.push(text); },
		dispose() { disposed++; },
	};
	return { predictor, learned, disposedCount: () => disposed };
}

function makeEditor(suffixFor?: (text: string) => string) {
	const stub = suffixFor ? stubPredictor(suffixFor) : undefined;
	const editor = new GentlePromptEditor(fakeTui as never, editorTheme as never, fakeKeybindings as never, {
		fg: (_role, text) => text, bold: (text) => text, requestRender() {}, pending: () => false, now: () => 0,
		doubleEscCancelEnabled: () => false, dispatchQueuedText() {},
		createPredictor: stub ? () => stub.predictor : undefined,
	} as never);
	return { editor, stub };
}

function type(editor: GentlePromptEditor, text: string): void {
	for (const ch of text) editor.handleInput(ch);
}

test("Tab accepts the ghost suggestion when one is shown", () => {
	const { editor } = makeEditor((text) => text === "hello" ? " world" : "");
	try {
		type(editor, "hello");
		assert.match(editor.render(40).join("\n"), /world/, "the ghost is rendered after the cursor");
		editor.handleInput("\t");
		assert.equal(editor.getText(), "hello world");
	} finally { editor.dispose(); }
});

test("Right accepts the ghost suggestion when one is shown", () => {
	const { editor } = makeEditor((text) => text === "hello" ? " world" : "");
	try {
		type(editor, "hello");
		editor.handleInput("\x1b[C");
		assert.equal(editor.getText(), "hello world");
	} finally { editor.dispose(); }
});

test("Tab without a ghost is forwarded unchanged", () => {
	const { editor } = makeEditor(() => "");
	try {
		type(editor, "hello");
		editor.handleInput("\t");
		assert.equal(editor.getText(), "hello", "no ghost means Tab inserts nothing from the predictor");
		assert.doesNotMatch(editor.render(40).join("\n"), /world/);
	} finally { editor.dispose(); }
});

test("Tab while the autocomplete dropdown is open is forwarded unchanged", () => {
	const { editor } = makeEditor((text) => text === "hello" ? " world" : "");
	try {
		type(editor, "hello");
		(editor as unknown as { isShowingAutocomplete(): boolean }).isShowingAutocomplete = () => true;
		editor.handleInput("\t");
		assert.notEqual(editor.getText(), "hello world", "the ghost is not accepted while autocomplete owns Tab");
	} finally { editor.dispose(); }
});

test("another key clears the ghost", () => {
	const { editor } = makeEditor((text) => text === "hello" ? " world" : "");
	try {
		type(editor, "hello");
		assert.match(editor.render(40).join("\n"), /world/);
		editor.handleInput("x");
		assert.equal(editor.getText(), "hellox");
		assert.doesNotMatch(editor.render(40).join("\n"), /world/, "the stale ghost is gone");
	} finally { editor.dispose(); }
});

test("rendering a ghost keeps every line within the width", () => {
	const { editor } = makeEditor(() => " a very long suggestion that would overflow the editor width easily");
	try {
		type(editor, "hi");
		const width = 24;
		// visibleWidth treats Pi's zero-width APC cursor markers correctly; stripAnsi
		// from terminal-theme leaves marker residue, so measure the painted width directly.
		for (const line of editor.render(width)) {
			assert.ok(visibleWidth(line) <= width, `line exceeds width: ${JSON.stringify(line)} -> ${visibleWidth(line)}`);
		}
		assert.match(stripAnsi(editor.render(width).join("\n")), /a very long/, "the ghost is painted");
		assert.equal(editor.getText(), "hi", "the stored text is never changed by the ghost");
	} finally { editor.dispose(); }
});

test("the submitted prompt is learned", () => {
	const { editor, stub } = makeEditor(() => "");
	try {
		editor.onSubmit = () => {};
		type(editor, "teach me");
		editor.handleInput("\r");
		assert.deepEqual(stub!.learned, ["teach me"]);
	} finally { editor.dispose(); }
});

test("without a predictor the ghost path is dormant (PRESERVE)", () => {
	const { editor } = makeEditor(); // no predictor
	try {
		type(editor, "hello");
		const before = editor.render(40).join("\n");
		editor.handleInput("\t");
		assert.notEqual(editor.getText(), "hello world", "no predictor means Tab never accepts a ghost");
		assert.doesNotMatch(before, /world/, "no ghost is ever rendered without a predictor");
	} finally { editor.dispose(); }
});
