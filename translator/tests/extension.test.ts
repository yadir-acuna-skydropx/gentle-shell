import test from "node:test";
import assert from "node:assert/strict";
import activate, { ENGLISH_ONLY_NOTE } from "../index.ts";

interface ClientOverrides {
	translate?: (q: string, source: string, target: string) => Promise<string>;
	detect?: (q: string) => Promise<{ language: string; confidence: number }[]>;
}
function fakeClient(overrides: ClientOverrides = {}) {
	return {
		translate: overrides.translate ?? (async (q: string) => `T(${q})`),
		detect: overrides.detect ?? (async () => [{ language: "es", confidence: 95 }]),
	};
}

function harness(opts: { client?: ReturnType<typeof fakeClient>; enabled?: boolean } = {}) {
	const handlers = new Map<string, (event: any, ctx: any) => any>();
	const commands = new Map<string, any>();
	const entryRenderers = new Map<string, any>();
	const appended: { type: string; data: any }[] = [];
	const statuses: Record<string, string | undefined> = {};
	const notifications: string[] = [];
	const pi = {
		on(event: string, handler: (event: any, ctx: any) => any) { handlers.set(event, handler); return () => {}; },
		registerCommand(name: string, options: any) { commands.set(name, options); },
		registerEntryRenderer(type: string, renderer: any) { entryRenderers.set(type, renderer); },
		registerMarkdownTransformer() {},
		appendEntry(type: string, data: any) { appended.push({ type, data }); },
	};
	const ctx = { hasUI: true, ui: { setStatus(k: string, v: string | undefined) { statuses[k] = v; }, notify(m: string) { notifications.push(m); } } };
	activate(pi as never, { client: opts.client ?? fakeClient(), enabled: opts.enabled ?? true });
	return { handlers, commands, entryRenderers, appended, statuses, notifications, ctx };
}

test("the input hook transforms a prose prompt into English", async () => {
	const h = harness({ client: fakeClient({ translate: async () => "hello" }) });
	const r = await h.handlers.get("input")!({ type: "input", text: "hola", source: "interactive" }, h.ctx);
	assert.deepEqual(r, { action: "transform", text: "hello" });
});

test("slash commands pass through", async () => {
	const h = harness();
	const r = await h.handlers.get("input")!({ type: "input", text: "/model", source: "interactive" }, h.ctx);
	assert.deepEqual(r, { action: "continue" });
});

test("extension-sourced input is skipped", async () => {
	const h = harness();
	const r = await h.handlers.get("input")!({ type: "input", text: "hola", source: "extension" }, h.ctx);
	assert.deepEqual(r, { action: "continue" });
});

test("a disabled extension passes input through", async () => {
	const h = harness({ enabled: false });
	const r = await h.handlers.get("input")!({ type: "input", text: "hola", source: "interactive" }, h.ctx);
	assert.deepEqual(r, { action: "continue" });
});

test("the English-only system note is appended exactly once when enabled", async () => {
	const h = harness();
	const opts = { appendSystemPrompt: "" };
	await h.handlers.get("before_agent_start")!({ systemPromptOptions: opts }, h.ctx);
	await h.handlers.get("before_agent_start")!({ systemPromptOptions: opts }, h.ctx);
	const occurrences = opts.appendSystemPrompt.split(ENGLISH_ONLY_NOTE).length - 1;
	assert.equal(occurrences, 1);
});

test("no system note is appended when disabled", async () => {
	const h = harness({ enabled: false });
	const opts = { appendSystemPrompt: "" };
	await h.handlers.get("before_agent_start")!({ systemPromptOptions: opts }, h.ctx);
	assert.equal(opts.appendSystemPrompt, "");
});

test("message_end translates an assistant reply into a display-only entry, ignoring thinking", async () => {
	const h = harness({ client: fakeClient({ translate: async () => "Hola" }) });
	await h.commands.get("translate").handler("es", h.ctx);
	await h.handlers.get("message_end")!({
		type: "message_end",
		message: { role: "assistant", content: [{ type: "text", text: "Hello" }, { type: "thinking", thinking: "secret" }] },
	}, h.ctx);
	assert.equal(h.appended.length, 1);
	assert.equal(h.appended[0].type, "gentle-translation");
	assert.ok(h.appended[0].data.text.includes("Hola"), h.appended[0].data.text);
	assert.ok(!h.appended[0].data.text.includes("secret"), h.appended[0].data.text);
	const renderer = h.entryRenderers.get("gentle-translation");
	assert.ok(renderer, "entry renderer registered");
	const lines = renderer({ data: h.appended[0].data }, { expanded: false }, {}).render(80);
	assert.ok(lines.join("\n").includes("Hola"));
});

test("message_end ignores non-assistant messages", async () => {
	const h = harness();
	await h.commands.get("translate").handler("es", h.ctx);
	await h.handlers.get("message_end")!({ type: "message_end", message: { role: "user", content: "x" } }, h.ctx);
	assert.equal(h.appended.length, 0);
});

test("/translate off then on toggles the input hook", async () => {
	const h = harness({ client: fakeClient({ translate: async () => "hello" }) });
	const cmd = h.commands.get("translate");
	await cmd.handler("off", h.ctx);
	assert.deepEqual(await h.handlers.get("input")!({ type: "input", text: "hola", source: "interactive" }, h.ctx), { action: "continue" });
	await cmd.handler("on", h.ctx);
	assert.deepEqual(await h.handlers.get("input")!({ type: "input", text: "hola", source: "interactive" }, h.ctx), { action: "transform", text: "hello" });
});

test("/translate with no argument reports the state and does not throw", async () => {
	const h = harness();
	await h.commands.get("translate").handler("", h.ctx);
	assert.ok(h.notifications.length >= 1);
});

test("/translate with an invalid argument notifies and does not throw", async () => {
	const h = harness();
	await h.commands.get("translate").handler("!!bad", h.ctx);
	assert.ok(h.notifications.some((n) => /translate/i.test(n)));
});

test("/translate argument completions include on, off and language codes", async () => {
	const h = harness();
	const items = await h.commands.get("translate").getArgumentCompletions("o");
	const values = items.map((i: { value: string }) => i.value);
	assert.ok(values.includes("on"));
	assert.ok(values.includes("off"));
	const all = await h.commands.get("translate").getArgumentCompletions("");
	assert.ok(all.some((i: { value: string }) => i.value === "es"));
});
