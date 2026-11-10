// Gentle Translator — a standalone Pi extension (loaded with `-e`, never placed
// under the agent's extensions dir) that keeps the whole model conversation in
// English while the user reads and writes in their own language:
//   - input hook:      user prompt -> English before the agent sees it
//   - system note:     tells the model to think and answer in English
//   - reply display:   the finalized English reply is translated to the user's
//                      language and shown as a display-only entry (outside the
//                      LLM context), so the model context stays English
//   - /translate:      on | off | <lang>, plus a footer status
// Everything fails open: when LibreTranslate is offline the original text is
// shown unchanged and the status reads "offline".
//
// Display mechanism: pi.appendEntry + pi.registerEntryRenderer. Custom entries
// explicitly "do not participate in LLM context" (see ExtensionAPI docs), which
// is exactly what display-only translation needs. The markdown-transformer
// route was rejected: MarkdownTransformContext.messageId is undefined for live
// messages (per types.d.ts), so a cache keyed by messageId cannot target the
// just-finalized reply, and the API exposes no synchronous re-render hook to
// re-run the transformer after the async translation lands.

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { AutocompleteItem } from "@earendil-works/pi-tui";
import { appendSystemPromptOnce } from "../lib/append-system-prompt.ts";
import { createClient, type TranslatorClient } from "./lib/libretranslate.ts";
import { createEngine } from "./lib/engine.ts";
import { DisplayCache } from "./lib/display-cache.ts";

export const ENGLISH_ONLY_NOTE =
	"This session uses an automatic translation layer. The user's prompts are translated to English before you see them, and your replies are translated back into the user's language for display only. Always think and respond in English. Never translate your own output, and keep code, inline code, commands, file paths, and URLs in their original form.";

const ENTRY_TYPE = "gentle-translation";
const STATUS_KEY = "translate";
const DEFAULT_URL = "http://127.0.0.1:5000";
const LANG_CODE = /^[a-z]{2}(?:-[a-z]{2})?$/i;

const LANGUAGE_COMPLETIONS: AutocompleteItem[] = [
	{ value: "on", label: "on", description: "Enable translation" },
	{ value: "off", label: "off", description: "Disable translation" },
	{ value: "en", label: "en", description: "English (no translation)" },
	{ value: "es", label: "es", description: "Spanish" },
];

export interface ActivateDeps {
	client: TranslatorClient;
	enabled: boolean;
}

function defaultDeps(): ActivateDeps {
	const env = globalThis.process?.env ?? {};
	const url = env.GENTLE_TRANSLATE_URL || DEFAULT_URL;
	const client = createClient({
		url,
		fetch: (input, init) => (globalThis.fetch as unknown as (u: string, i?: unknown) => Promise<never>)(input, init),
	});
	return { client, enabled: env.GENTLE_TRANSLATE !== "off" };
}

// Only the plain text of an assistant message is translatable prose. Thinking
// blocks and tool calls are deliberately excluded (never translated).
function assistantText(message: { role?: string; content?: unknown }): string {
	if (!message || message.role !== "assistant" || !Array.isArray(message.content)) return "";
	return message.content
		.filter((part): part is { type: "text"; text: string } => !!part && (part as { type?: string }).type === "text" && typeof (part as { text?: unknown }).text === "string")
		.map((part) => part.text)
		.join("");
}

function wrap(text: string, width: number): string[] {
	const lines: string[] = [];
	for (const paragraph of text.split("\n")) {
		if (paragraph.length <= width || width <= 0) {
			lines.push(paragraph);
			continue;
		}
		let rest = paragraph;
		while (rest.length > width) {
			let cut = rest.lastIndexOf(" ", width);
			if (cut <= 0) cut = width;
			lines.push(rest.slice(0, cut));
			rest = rest.slice(cut).replace(/^ /, "");
		}
		lines.push(rest);
	}
	return lines;
}

export default function activate(pi: ExtensionAPI, deps: ActivateDeps = defaultDeps()): void {
	const engine = createEngine({ client: deps.client, enabled: deps.enabled });
	const cache = new DisplayCache();
	let replyCounter = 0;

	function updateStatus(ctx: Pick<ExtensionContext, "hasUI" | "ui">): void {
		if (!ctx?.hasUI) return;
		ctx.ui.setStatus(STATUS_KEY, `translate: ${engine.statusText()}`);
	}

	// Prompt -> English. Extension-sourced input is skipped so a translated
	// prompt we re-inject is never translated again.
	pi.on("input", async (event, ctx) => {
		if (event.source === "extension") return { action: "continue" };
		const result = await engine.prepareInput(event.text);
		updateStatus(ctx);
		if (!result.translated) return { action: "continue" };
		return { action: "transform", text: result.text };
	});

	// English-only instruction, appended once per build and only while enabled.
	pi.on("before_agent_start", (event) => {
		if (!engine.isEnabled()) return;
		appendSystemPromptOnce(event.systemPromptOptions as { appendSystemPrompt: string }, ENGLISH_ONLY_NOTE);
	});

	// Finalized assistant reply -> user language, shown as a display-only entry.
	pi.on("message_end", async (event, ctx) => {
		if (!engine.isEnabled()) return;
		const text = assistantText(event.message as { role?: string; content?: unknown });
		if (text.trim().length === 0) return;
		const translated = await engine.translateReply(text);
		updateStatus(ctx);
		if (translated === text) return; // same language, offline, or nothing to do
		const id = `reply-${++replyCounter}`;
		cache.set(id, { original: text, translated });
		pi.appendEntry(ENTRY_TYPE, { id, text: translated });
	});

	// Display-only renderer for the translated entry. It reads the cache first
	// (translated text for this id) and falls back to the entry's own data.
	pi.registerEntryRenderer(ENTRY_TYPE, (entry: { data?: { id?: string; text?: string } }) => {
		const data = entry?.data ?? {};
		const text = data.id ? cache.display(data.id, data.text ?? "") : data.text ?? "";
		return {
			render(width: number): string[] {
				return wrap(text, width);
			},
			invalidate(): void {},
		};
	});

	pi.registerCommand("translate", {
		description: "Translate prompts to English and replies back to your language (LibreTranslate).",
		getArgumentCompletions(prefix: string) {
			const lower = prefix.toLowerCase();
			return LANGUAGE_COMPLETIONS.filter((item) => item.value.startsWith(lower));
		},
		async handler(args: string, ctx: ExtensionContext) {
			const arg = args.trim().toLowerCase();
			if (arg === "") {
				ctx.ui.notify(`translate: ${engine.statusText()}${engine.getUserLang() ? "" : " (no language set yet)"}`, "info");
				updateStatus(ctx);
				return;
			}
			if (arg === "on") {
				engine.setEnabled(true);
				ctx.ui.notify("translate: on", "info");
			} else if (arg === "off") {
				engine.setEnabled(false);
				ctx.ui.notify("translate: off", "info");
			} else if (LANG_CODE.test(arg)) {
				engine.setUserLang(arg);
				engine.setEnabled(true);
				ctx.ui.notify(`translate: ${arg}`, "info");
			} else {
				ctx.ui.notify("translate: expected on, off, or a language code like es", "warning");
				return;
			}
			updateStatus(ctx);
		},
	});
}
