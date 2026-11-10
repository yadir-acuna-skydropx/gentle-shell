import test from "node:test";
import assert from "node:assert/strict";
import { segment, reassemble, translateMarkdown } from "../lib/segmenter.ts";

test("prose-only input is a single unprotected segment", () => {
	assert.deepEqual(segment("hola mundo"), [{ protected: false, text: "hola mundo" }]);
});

test("fenced code blocks are protected and reassemble byte-identical", () => {
	const md = "antes\n```js\nconst a = 1;\n```\ndespues";
	const segs = segment(md);
	assert.equal(reassemble(segs), md);
	assert.ok(segs.some((s) => s.protected && s.text.includes("const a = 1;")));
	for (const s of segs) if (!s.protected) assert.ok(!s.text.includes("```"), s.text);
});

test("code-only input stays byte-identical", () => {
	const md = "```\nraw\n```";
	const segs = segment(md);
	assert.equal(reassemble(segs), md);
	assert.equal(segs.length, 1);
	assert.equal(segs[0].protected, true);
});

test("inline code, URLs and paths are protected and reassemble byte-identical", () => {
	const md = "corre `npm test` ve a https://example.com y abre ./src/index.ts y translator/index.ts porfa";
	const segs = segment(md);
	assert.equal(reassemble(segs), md);
	const prot = segs.filter((s) => s.protected).map((s) => s.text);
	assert.ok(prot.includes("`npm test`"), JSON.stringify(prot));
	assert.ok(prot.includes("https://example.com"), JSON.stringify(prot));
	assert.ok(prot.includes("./src/index.ts"), JSON.stringify(prot));
	assert.ok(prot.includes("translator/index.ts"), JSON.stringify(prot));
});

test("translateMarkdown translates only prose and keeps protected text verbatim", async () => {
	const md = "hola `code()` mundo";
	const out = await translateMarkdown(md, async (t) => t.toUpperCase());
	assert.equal(out, "HOLA `code()` MUNDO");
});

test("translateMarkdown with identity is byte-identical for mixed content", async () => {
	const md = "texto ```\nx\n``` fin https://a.b/c y ./p/q.ts";
	assert.equal(await translateMarkdown(md, async (t) => t), md);
});

test("translateMarkdown preserves whitespace around prose between protected spans", async () => {
	const md = "uno `x` dos";
	const out = await translateMarkdown(md, async (t) => `[${t}]`);
	assert.equal(out, "[uno] `x` [dos]");
});
