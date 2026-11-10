import test from "node:test";
import assert from "node:assert/strict";
import { createClient } from "../lib/libretranslate.ts";

function jsonResponse(data: unknown, ok = true, status = 200) {
	return { ok, status, async json() { return data; } };
}
// A delay that never settles, so the real fetch always wins the timeout race.
const neverDelay = () => new Promise<void>(() => {});

test("translate posts to /translate and returns translatedText", async () => {
	const calls: { url: string; body: unknown }[] = [];
	const client = createClient({
		fetch: async (url: string, init?: { body?: string }) => {
			calls.push({ url, body: JSON.parse(init?.body ?? "{}") });
			return jsonResponse({ translatedText: "hello" });
		},
		url: "http://host:5000",
		delay: neverDelay,
	});
	assert.equal(await client.translate("hola", "es", "en"), "hello");
	assert.equal(calls[0].url, "http://host:5000/translate");
	assert.deepEqual(calls[0].body, { q: "hola", source: "es", target: "en", format: "text" });
});

test("detect posts to /detect and returns the array", async () => {
	let seen = "";
	const client = createClient({
		fetch: async (url: string) => { seen = url; return jsonResponse([{ language: "es", confidence: 95 }]); },
		url: "http://host:5000",
		delay: neverDelay,
	});
	assert.deepEqual(await client.detect("hola"), [{ language: "es", confidence: 95 }]);
	assert.equal(seen, "http://host:5000/detect");
});

test("a non-200 response throws", async () => {
	const client = createClient({ fetch: async () => jsonResponse({}, false, 500), delay: neverDelay });
	await assert.rejects(() => client.translate("x", "es", "en"));
});

test("a hung fetch is rejected once the timeout fires", async () => {
	const client = createClient({ fetch: () => new Promise(() => {}), timeoutMs: 10, delay: () => Promise.resolve() });
	await assert.rejects(() => client.translate("x", "es", "en"), /timeout/i);
});

test("the default base URL is 127.0.0.1:5000", async () => {
	let seen = "";
	const client = createClient({ fetch: async (u: string) => { seen = u; return jsonResponse({ translatedText: "" }); }, delay: neverDelay });
	await client.translate("a", "auto", "en");
	assert.equal(seen, "http://127.0.0.1:5000/translate");
});
