// Markdown segmenter: splits text into translatable prose and protected spans
// (code, URLs, file paths) so a translator can rewrite only the prose and
// reassemble the original byte-for-byte when nothing is translated.
//
// The segments always partition the input exactly: concatenating every
// segment's `text` in order reproduces the original string. That invariant is
// what lets `translateMarkdown` keep code blocks, inline code, URLs and paths
// verbatim while the prose around them changes.

export interface Segment {
	/** True for spans that must never be translated (code, URLs, paths). */
	protected: boolean;
	text: string;
}

// Alternation order matters: at any position the regex engine tries these
// left-to-right, so fenced blocks win over inline code, and URLs win over the
// path patterns that would otherwise swallow a URL's slashes.
const PROTECTED = new RegExp(
	[
		"```[\\s\\S]*?```", // ``` fenced code block
		"~~~[\\s\\S]*?~~~", // ~~~ fenced code block
		"`[^`\\n]+`", // `inline code`
		"(?:https?://|www\\.)[^\\s<>`]+", // URLs
		"(?:~|\\.{1,2})?/[^\\s`<>]+", // absolute/relative paths: /a, ./a, ../a, ~/a
		"[\\w.-]+/[\\w./-]+", // bare relative paths: a/b, translator/index.ts
		"[\\w-]+\\.(?:ts|tsx|js|jsx|mjs|cjs|json|md|markdown|rs|py|go|sh|bash|toml|yml|yaml|lock|txt|html|css)\\b", // file.ext
	].join("|"),
	"g",
);

export function segment(md: string): Segment[] {
	const segments: Segment[] = [];
	let last = 0;
	PROTECTED.lastIndex = 0;
	let match: RegExpExecArray | null;
	while ((match = PROTECTED.exec(md)) !== null) {
		if (match[0].length === 0) {
			PROTECTED.lastIndex += 1; // never loop forever on a zero-width match
			continue;
		}
		if (match.index > last) segments.push({ protected: false, text: md.slice(last, match.index) });
		segments.push({ protected: true, text: match[0] });
		last = match.index + match[0].length;
	}
	if (last < md.length) segments.push({ protected: false, text: md.slice(last) });
	if (segments.length === 0) segments.push({ protected: false, text: md });
	return segments;
}

export function reassemble(segments: Segment[]): string {
	return segments.map((s) => s.text).join("");
}

// Translate only prose segments, preserving the surrounding whitespace so the
// spacing between protected spans is unchanged. Protected spans and
// whitespace-only prose pass through untouched.
export async function translateMarkdown(md: string, translateProse: (text: string) => Promise<string>): Promise<string> {
	const segments = segment(md);
	const translated = await Promise.all(
		segments.map(async (s) => {
			if (s.protected) return s.text;
			const lead = s.text.match(/^\s*/)?.[0] ?? "";
			const trail = s.text.match(/\s*$/)?.[0] ?? "";
			const core = s.text.slice(lead.length, s.text.length - trail.length);
			if (core.length === 0) return s.text;
			return `${lead}${await translateProse(core)}${trail}`;
		}),
	);
	return translated.join("");
}
