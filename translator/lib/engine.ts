// Translator engine: the pure state machine behind the extension. It owns the
// enabled flag, the remembered user language, and the offline (fail-open)
// state, and exposes two operations over an injected LibreTranslate client:
//   - prepareInput: user prompt -> English (for the model)
//   - translateReply: finalized English reply -> the user's language (display)
// Every engine call fails open: if the client is unreachable, slow, or errors,
// the original text passes through unchanged and the engine goes offline until
// the next success.

import { translateMarkdown } from "./segmenter.ts";
import type { TranslatorClient } from "./libretranslate.ts";

export interface EngineOptions {
	client: TranslatorClient;
	/** Whether translation is enabled at start (default true). */
	enabled?: boolean;
	/** Minimum detection confidence (0-100) to translate and remember (default 50). */
	minConfidence?: number;
}

export interface PreparedInput {
	/** True when the text was actually translated to English. */
	translated: boolean;
	text: string;
}

export interface TranslatorEngine {
	isEnabled(): boolean;
	setEnabled(enabled: boolean): void;
	getUserLang(): string | null;
	setUserLang(lang: string): void;
	isOffline(): boolean;
	statusText(): string;
	prepareInput(text: string): Promise<PreparedInput>;
	translateReply(text: string): Promise<string>;
}

export function createEngine(options: EngineOptions): TranslatorEngine {
	const { client } = options;
	const minConfidence = options.minConfidence ?? 50;
	let enabled = options.enabled ?? true;
	let userLang: string | null = null;
	let offline = false;

	function markOnline(): void {
		offline = false;
	}
	function markOffline(): void {
		offline = true;
	}

	// Resolve the source language for a prompt, detecting and remembering it on
	// the first prompt. Returns null to signal "do not translate this prompt"
	// (English, low confidence, or a detection failure — which also goes offline).
	async function resolveSourceLang(text: string): Promise<string | null> {
		if (userLang !== null) return userLang === "en" ? null : userLang;
		let detected: { language: string; confidence: number }[];
		try {
			detected = await client.detect(text);
		} catch {
			markOffline();
			return null;
		}
		markOnline();
		const top = detected[0];
		if (!top || top.confidence < minConfidence) return null; // retry detection next prompt
		userLang = top.language;
		return top.language === "en" ? null : top.language;
	}

	return {
		isEnabled() {
			return enabled;
		},
		setEnabled(value) {
			enabled = value;
		},
		getUserLang() {
			return userLang;
		},
		setUserLang(lang) {
			userLang = lang;
		},
		isOffline() {
			return offline;
		},
		statusText() {
			if (!enabled) return "off";
			if (offline) return "offline";
			return userLang ?? "on";
		},

		async prepareInput(text) {
			if (!enabled) return { translated: false, text };
			// A slash command is never a prompt to translate.
			if (text.startsWith("/")) return { translated: false, text };
			const source = await resolveSourceLang(text);
			if (source === null) return { translated: false, text };
			try {
				const out = await translateMarkdown(text, (prose) => client.translate(prose, source, "en"));
				markOnline();
				return { translated: true, text: out };
			} catch {
				markOffline();
				return { translated: false, text };
			}
		},

		async translateReply(text) {
			if (!enabled) return text;
			if (userLang === null || userLang === "en") return text;
			try {
				const out = await translateMarkdown(text, (prose) => client.translate(prose, "en", userLang as string));
				markOnline();
				return out;
			} catch {
				markOffline();
				return text;
			}
		},
	};
}
