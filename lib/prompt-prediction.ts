// Inline next-text prediction client for the gentle-shell prompt editor (S9).
//
// The gentle-partitioner (T8a) runs an in-process n-gram engine and serves it on
// 127.0.0.1 with a bearer token, passing GENTLE_PREDICT_URL and
// GENTLE_PREDICT_TOKEN to the gentle-shell child. This module is the child-side
// client: a pure, dependency-injected wrapper around that HTTP protocol so the
// editor never performs network work inline and the whole feature is a no-op when
// the environment variables are absent.
//
// Protocol (see partitioner/src/predict_server.rs):
// - POST {url}/predict  {"text":"<before cursor>","mode":"chat"} -> 200 {"suggestion":"<suffix>"}
// - POST {url}/learn    {"text":"...","mode":"chat"}             -> 204
// - Authorization: Bearer <token> required; any error/timeout/non-2xx yields no
//   suggestion and never throws.

/** Minimal shape of the fetch Response this client consumes. */
export interface PredictResponse {
	ok: boolean;
	status: number;
	json(): Promise<unknown>;
}

/** Fetch signature accepted by the client; `globalThis.fetch` satisfies it. */
export type PredictFetch = (
	url: string,
	init: { method: string; headers: Record<string, string>; body: string; signal?: AbortSignal },
) => Promise<PredictResponse>;

/** Injectable timers so tests drive the debounce and timeout deterministically. */
export interface PredictTimers {
	setTimeout: (handler: () => void, ms: number) => unknown;
	clearTimeout: (handle: unknown) => void;
}

export interface PromptPredictorConfig extends PredictTimers {
	url: string;
	token: string;
	fetch: PredictFetch;
	/** Delay before a keystroke burst becomes a single request. */
	debounceMs?: number;
	/** Abort a request that has not answered within this budget. */
	timeoutMs?: number;
}

export interface PromptPredictor {
	/**
	 * Debounced suggestion request. A burst of calls collapses to one fetch for the
	 * latest text; a newer request aborts the previous in-flight one; a stale
	 * response is ignored. `onSuggestion` runs only for the latest text and only
	 * with a non-empty suffix. Never throws.
	 */
	request(text: string, onSuggestion: (suffix: string) => void): void;
	/** Fire-and-forget: teach the engine a submitted prompt. Never throws. */
	learn(text: string): void;
	/** Cancel the pending debounce and any in-flight request; further calls are inert. */
	dispose(): void;
}

export function createPromptPredictor(config: PromptPredictorConfig): PromptPredictor {
	const { url, token, fetch: fetchFn, setTimeout: setT, clearTimeout: clearT } = config;
	const debounceMs = config.debounceMs ?? 120;
	const timeoutMs = config.timeoutMs ?? 300;

	let debounceHandle: unknown;
	let pending: { text: string; onSuggestion: (suffix: string) => void } | undefined;
	let inflight: AbortController | undefined;
	// Monotonic request id; a response is honored only while it is still the latest.
	let latest = 0;
	let disposed = false;

	const headers = (): Record<string, string> => ({
		"Content-Type": "application/json",
		Authorization: `Bearer ${token}`,
	});

	const fire = (): void => {
		debounceHandle = undefined;
		const job = pending;
		pending = undefined;
		if (!job || disposed) return;
		// A newer request supersedes the previous one: abort it so the engine and
		// the socket are not kept busy for a suggestion that can no longer apply.
		inflight?.abort();
		const controller = new AbortController();
		inflight = controller;
		const id = ++latest;
		let timeoutHandle: unknown = setT(() => {
			try { controller.abort(); } catch { /* already settled */ }
		}, timeoutMs);
		const settle = (): void => {
			if (timeoutHandle !== undefined) { clearT(timeoutHandle); timeoutHandle = undefined; }
			if (inflight === controller) inflight = undefined;
		};
		void (async () => {
			try {
				const res = await fetchFn(`${url}/predict`, {
					method: "POST",
					headers: headers(),
					body: JSON.stringify({ text: job.text, mode: "chat" }),
					signal: controller.signal,
				});
				if (!res.ok) return;
				const data = await res.json();
				const suffix = data && typeof data === "object" ? (data as { suggestion?: unknown }).suggestion : undefined;
				// Ignore a stale response; an empty suffix means "no suggestion".
				if (typeof suffix === "string" && suffix.length > 0 && id === latest && !disposed) job.onSuggestion(suffix);
			} catch {
				// Abort, timeout, network failure or bad JSON: no suggestion, no throw.
			} finally {
				settle();
			}
		})();
	};

	return {
		request(text, onSuggestion) {
			if (disposed) return;
			pending = { text, onSuggestion };
			if (debounceHandle !== undefined) clearT(debounceHandle);
			debounceHandle = setT(fire, debounceMs);
		},
		learn(text) {
			if (disposed) return;
			try {
				void Promise.resolve(
					fetchFn(`${url}/learn`, { method: "POST", headers: headers(), body: JSON.stringify({ text, mode: "chat" }) }),
				).catch(() => { /* fire and forget */ });
			} catch {
				// Never let a learn attempt surface to the caller.
			}
		},
		dispose() {
			disposed = true;
			if (debounceHandle !== undefined) { clearT(debounceHandle); debounceHandle = undefined; }
			pending = undefined;
			inflight?.abort();
			inflight = undefined;
		},
	};
}

/** Timer/fetch overrides for {@link fromEnv}; production defaults to the globals. */
export interface FromEnvOverrides extends Partial<PredictTimers> {
	fetch?: PredictFetch;
	debounceMs?: number;
	timeoutMs?: number;
}

/**
 * Build a predictor from the partitioner-provided environment, or return `null`
 * when GENTLE_PREDICT_URL or GENTLE_PREDICT_TOKEN is missing. A `null` return is
 * the signal to the editor that the inline-suggestion code path stays dormant, so
 * behavior is byte-for-byte identical to a gentle-shell launched outside the
 * partitioner.
 */
export function fromEnv(env: NodeJS.ProcessEnv, overrides: FromEnvOverrides = {}): PromptPredictor | null {
	const url = env.GENTLE_PREDICT_URL;
	const token = env.GENTLE_PREDICT_TOKEN;
	if (!url || !token) return null;
	return createPromptPredictor({
		url,
		token,
		fetch: overrides.fetch ?? ((input, init) => globalThis.fetch(input, init as RequestInit) as Promise<PredictResponse>),
		setTimeout: overrides.setTimeout ?? ((handler, ms) => setTimeout(handler, ms)),
		clearTimeout: overrides.clearTimeout ?? ((handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)),
		debounceMs: overrides.debounceMs,
		timeoutMs: overrides.timeoutMs,
	});
}
