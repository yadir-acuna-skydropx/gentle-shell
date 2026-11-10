// Display cache for finalized assistant replies. The extension translates a
// reply once (when it finalizes) and stores it here keyed by a stable id; the
// display mechanism returns the translated text for that id and the original
// text for anything not cached. This keeps the model context English: the
// cache only feeds display, never the LLM.

export interface DisplayEntry {
	original: string;
	translated: string;
}

export class DisplayCache {
	private readonly entries = new Map<string, DisplayEntry>();

	set(id: string, entry: DisplayEntry): void {
		this.entries.set(id, entry);
	}

	get(id: string): DisplayEntry | undefined {
		return this.entries.get(id);
	}

	/** The text to show for `id`: the cached translation, or `original` when absent. */
	display(id: string, original: string): string {
		return this.entries.get(id)?.translated ?? original;
	}
}
