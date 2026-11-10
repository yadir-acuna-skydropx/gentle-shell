// Minimal LibreTranslate HTTP client over an injected `fetch` and clock, so it
// is fully unit-testable without a network. Endpoints:
//   POST /translate  {q, source, target, format:"text"} -> {translatedText}
//   POST /detect     {q}                                 -> [{language, confidence}]
// The default engine runs on http://127.0.0.1:5000 (everything stays local).

export interface FetchResponse {
	ok: boolean;
	status: number;
	json(): Promise<unknown>;
}
export type FetchLike = (url: string, init?: { method?: string; headers?: Record<string, string>; body?: string; signal?: AbortSignal }) => Promise<FetchResponse>;

export interface DetectResult {
	language: string;
	confidence: number;
}

export interface TranslatorClient {
	translate(q: string, source: string, target: string): Promise<string>;
	detect(q: string): Promise<DetectResult[]>;
}

export interface ClientOptions {
	fetch: FetchLike;
	/** Base URL; defaults to http://127.0.0.1:5000. A trailing slash is ignored. */
	url?: string;
	/** Request timeout in milliseconds (default 4000). */
	timeoutMs?: number;
	/** Injected delay used for the timeout race; defaults to setTimeout. */
	delay?: (ms: number) => Promise<void>;
}

const DEFAULT_URL = "http://127.0.0.1:5000";
const DEFAULT_TIMEOUT_MS = 4000;

export function createClient(options: ClientOptions): TranslatorClient {
	const base = (options.url ?? DEFAULT_URL).replace(/\/+$/, "");
	const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
	const delay = options.delay ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));

	async function post(path: string, payload: unknown): Promise<unknown> {
		const controller = new AbortController();
		const timeout = delay(timeoutMs).then(() => {
			controller.abort();
			throw new Error(`translate timeout after ${timeoutMs}ms`);
		});
		const request = options.fetch(`${base}${path}`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify(payload),
			signal: controller.signal,
		}).then(async (res) => {
			if (!res.ok) throw new Error(`${path} responded ${res.status}`);
			return res.json();
		});
		return Promise.race([request, timeout]);
	}

	return {
		async translate(q, source, target) {
			const data = (await post("/translate", { q, source, target, format: "text" })) as { translatedText?: string };
			return data.translatedText ?? "";
		},
		async detect(q) {
			const data = (await post("/detect", { q })) as DetectResult[];
			return Array.isArray(data) ? data : [];
		},
	};
}
