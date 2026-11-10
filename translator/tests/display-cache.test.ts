import test from "node:test";
import assert from "node:assert/strict";
import { DisplayCache } from "../lib/display-cache.ts";

test("display returns the translated text for a known id and the original otherwise", () => {
	const cache = new DisplayCache();
	cache.set("m1", { original: "Hello", translated: "Hola" });
	assert.equal(cache.display("m1", "Hello"), "Hola");
	assert.equal(cache.display("m2", "Other"), "Other");
	assert.deepEqual(cache.get("m1"), { original: "Hello", translated: "Hola" });
	assert.equal(cache.get("m2"), undefined);
});
