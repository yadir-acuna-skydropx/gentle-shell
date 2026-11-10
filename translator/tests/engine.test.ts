import test from "node:test";
import assert from "node:assert/strict";
import { createEngine } from "../lib/engine.ts";

interface ClientOverrides {
	translate?: (q: string, source: string, target: string) => Promise<string>;
	detect?: (q: string) => Promise<{ language: string; confidence: number }[]>;
}
function fakeClient(overrides: ClientOverrides = {}) {
	return {
		translate: overrides.translate ?? (async (q: string) => `EN(${q})`),
		detect: overrides.detect ?? (async () => [{ language: "es", confidence: 95 }]),
	};
}

test("a disabled engine passes input through untouched", async () => {
	const e = createEngine({ client: fakeClient(), enabled: false });
	assert.deepEqual(await e.prepareInput("hola"), { translated: false, text: "hola" });
});

test("a slash command passes through untouched", async () => {
	const e = createEngine({ client: fakeClient() });
	assert.deepEqual(await e.prepareInput("/translate on"), { translated: false, text: "/translate on" });
});

test("the first prompt is detected, remembered and translated to English", async () => {
	const e = createEngine({ client: fakeClient({ translate: async (q) => (q === "hola" ? "hello" : `EN(${q})`) }) });
	const r = await e.prepareInput("hola");
	assert.deepEqual(r, { translated: true, text: "hello" });
	assert.equal(e.getUserLang(), "es");
});

test("an English prompt passes through and English is remembered", async () => {
	const e = createEngine({ client: fakeClient({ detect: async () => [{ language: "en", confidence: 99 }] }) });
	const r = await e.prepareInput("hello there");
	assert.equal(r.translated, false);
	assert.equal(e.getUserLang(), "en");
});

test("a low-confidence detection does not translate and is not remembered", async () => {
	const e = createEngine({ client: fakeClient({ detect: async () => [{ language: "es", confidence: 12 }] }) });
	const r = await e.prepareInput("ok");
	assert.equal(r.translated, false);
	assert.equal(e.getUserLang(), null);
});

test("an explicit user language skips detection", async () => {
	let detects = 0;
	const e = createEngine({ client: fakeClient({ detect: async () => { detects++; return []; }, translate: async () => "hello" }) });
	e.setUserLang("es");
	const r = await e.prepareInput("hola");
	assert.deepEqual(r, { translated: true, text: "hello" });
	assert.equal(detects, 0);
});

test("protected code in a prompt is not translated", async () => {
	const e = createEngine({ client: fakeClient({ translate: async (q) => q.toUpperCase() }) });
	e.setUserLang("es");
	const r = await e.prepareInput("hola `code()` mundo");
	assert.equal(r.text, "HOLA `code()` MUNDO");
});

test("the reply is translated to the user language with code preserved", async () => {
	const e = createEngine({ client: fakeClient({ translate: async (q) => (q === "Hello" ? "Hola" : `ES(${q})`) }) });
	e.setUserLang("es");
	const out = await e.translateReply("Hello `keep()` world");
	assert.ok(out.includes("`keep()`"), out);
	assert.ok(out.startsWith("Hola"), out);
});

test("the reply passes through when the user language is English", async () => {
	const e = createEngine({ client: fakeClient() });
	e.setUserLang("en");
	assert.equal(await e.translateReply("Hello"), "Hello");
});

test("the reply passes through when no user language is known", async () => {
	const e = createEngine({ client: fakeClient() });
	assert.equal(await e.translateReply("Hello"), "Hello");
});

test("a translation failure fails open, goes offline, and recovers on the next success", async () => {
	let fail = true;
	const e = createEngine({ client: fakeClient({ translate: async (q) => { if (fail) throw new Error("down"); return "hello"; } }) });
	e.setUserLang("es");
	const r1 = await e.prepareInput("hola");
	assert.deepEqual(r1, { translated: false, text: "hola" });
	assert.equal(e.isOffline(), true);
	assert.equal(e.statusText(), "offline");
	fail = false;
	const r2 = await e.prepareInput("hola");
	assert.deepEqual(r2, { translated: true, text: "hello" });
	assert.equal(e.isOffline(), false);
});

test("a detect failure fails open and goes offline", async () => {
	const e = createEngine({ client: fakeClient({ detect: async () => { throw new Error("down"); } }) });
	const r = await e.prepareInput("hola");
	assert.deepEqual(r, { translated: false, text: "hola" });
	assert.equal(e.isOffline(), true);
});

test("statusText reflects enabled, language and disabled states", () => {
	const e = createEngine({ client: fakeClient() });
	assert.equal(e.statusText(), "on");
	e.setUserLang("es");
	assert.equal(e.statusText(), "es");
	e.setEnabled(false);
	assert.equal(e.statusText(), "off");
});
