# Gentle Translator

A standalone [Pi](https://github.com/earendil-works/pi) extension (the S8
"translator engine" of gentle-partitioner) that lets you chat with the agent in
your own language while **the whole model conversation stays in English** —
every message pivots through English, exactly as the user asked ("el usario
simpre la va a ver en su idioma nativo pero todo pasa por ingles").

It is deliberately **not** installed under the agent's `extensions/` directory,
so the existing gentle-shell setup is left untouched (L10: "Extensión Pi
separada"). You load it explicitly with `-e`.

## What it does

- **Prompt → English.** Your prompt is detected and translated to English
  before the agent sees it. Code fences, inline code, URLs, file paths, and
  slash commands (anything starting with `/`) are never translated.
- **English-only agent.** A system note (appended once, via
  `appendSystemPromptOnce`) tells the model to think and answer in English.
- **Reply → your language.** When an assistant reply finishes, its prose is
  translated into your language and shown as a **display-only** entry. The
  model context stays English; the translation never re-enters the LLM.
- **Fail-open.** If LibreTranslate is unreachable, slow (~4s timeout), or
  errors, the original text passes through unchanged and the status reads
  `translate: offline`. It recovers automatically on the next success.

### Display mechanism (and why)

Reply translation uses `pi.appendEntry` + `pi.registerEntryRenderer`. Custom
entries explicitly *do not participate in the LLM context*, which is exactly
what display-only translation needs.

The alternative — `pi.registerMarkdownTransformer` with a cache keyed by
`messageId` — was rejected: `MarkdownTransformContext.messageId` is `undefined`
for live messages (per `dist/core/extensions/types.d.ts`), so a cache keyed by
`messageId` cannot target the just-finalized reply, and the extension API
exposes no synchronous re-render hook to re-run the transformer after the async
translation lands. The appended entry is deterministic and triggers its own
render.

**Limit:** while the reply is streaming you see it in English; the translation
appears as a separate entry once the reply finishes.

## Commands

- `/translate` — show the current state (enabled/offline and the language).
- `/translate on` / `/translate off` — enable or disable translation.
- `/translate <lang>` — set your language explicitly to an ISO code, e.g.
  `/translate es`. Otherwise the language is detected from your first prompt
  (and ignored if it is English or the confidence is below 50).

Argument completions are provided for `on`, `off`, and language codes. The
current state is also shown in the footer status bar (`translate: …`).

## Run LibreTranslate (the engine)

The engine is local — everything stays on `127.0.0.1`. Run it in Docker:

```sh
docker run -d --name gentle-translate --restart unless-stopped \
  -p 127.0.0.1:5000:5000 \
  -e LT_LOAD_ONLY=en,es \
  -v gentle-translate-models:/home/libretranslate/.local \
  libretranslate/libretranslate
```

Override the URL with `GENTLE_TRANSLATE_URL` (default `http://127.0.0.1:5000`).

## Load the extension

```sh
gentle-shell -e /home/yadir/gentle-shell/translator/index.ts
```

Passthrough of `-e` to Pi is handled by the launcher: parsed flags after the
launcher's own options are collected in `passthrough` and forwarded verbatim
with `args.push(...input.passthrough)` (`lib/gentle-shell-launcher.ts:1117`).

Alternatively, copy or symlink the `translator/` folder into the agent
extensions directory so it is discovered automatically, e.g.:

```sh
ln -s /home/yadir/gentle-shell/translator ~/.gentle-shell/agent/extensions/gentle-translator
```

### Enable/disable at start

- Enabled by default when loaded.
- `GENTLE_TRANSLATE=off gentle-shell -e …/translator/index.ts` starts disabled.

## Privacy

Everything stays on localhost: your prompts and the agent's replies are sent
only to the LibreTranslate instance you run on `127.0.0.1`. Nothing is sent to
any third-party translation service.

## Tests

```sh
node --experimental-strip-types --test translator/tests/*.test.ts
```

(Requires Node ≥ 22.6 for `--experimental-strip-types`.) Pure logic lives in
`translator/lib/*.ts` with an injected `fetch` and clock; the extension entry
is `translator/index.ts`.
