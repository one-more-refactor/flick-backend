# flick API — documentation for agents

Machine-readable description: [`{{BASE}}/openapi.json`]({{BASE}}/openapi.json)
· Authentication: [`{{BASE}}/auth.md`]({{BASE}}/auth.md) · Catalog:
[`{{BASE}}/.well-known/api-catalog`]({{BASE}}/.well-known/api-catalog)

The binding specification is
[`docs/CONTRACTS.md`](https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md)
in the umbrella repo. When this page and CONTRACTS.md disagree, CONTRACTS.md
wins. Everything here is the same API on the hosted service and on a
self-hosted container, so a client written once works against both.

## The shape of it

Base path `/api`. JSON in, JSON out. Errors are
`{"error": "human readable message"}` with a 4xx/5xx status. Authentication is
the `flick_session` HttpOnly cookie.

## Start here

Two requests get you from nothing to a reading timeline:

```sh
BASE={{BASE}}

# 1. mint an anonymous session — no registration, no credentials
curl -sS -c jar.txt -X POST "$BASE/api/auth/guest"

# 2. every new user is seeded with the public-domain catalog, so there is
#    already something to read
curl -sS -b jar.txt "$BASE/api/books" | head -c 400
```

Then pick a book id and fetch either its **timeline** (to play it) or its
**text** (to read it):

```sh
curl -sS -b jar.txt "$BASE/api/books/$ID/timeline"
curl -sS -b jar.txt "$BASE/api/books/$ID/text"
```

## The timeline is the interesting part

`GET /api/books/{id}/timeline` returns:

```json
{
  "version": 1,
  "word_count": 1841,
  "words": [["Whenever", 2, 1.14], ["I", 0, 0.55], ["find", 1, 0.92]]
}
```

Each entry is `[text, orp_index, weight]`:

- `text` — the token to display.
- `orp_index` — the 0-based index of the pivot letter, the character the eye
  should be anchored on.
- `weight` — a *relative* duration. Render the token for
  `weight * (60000 / wpm)` milliseconds.

Weights are why flick is not a metronome: rare words linger, common ones fly,
long words are split into chunks, and clause and sentence ends get wrap-up
pauses. Because the weights are normalised across the document, the wpm dial is
true throughput and not a nominal setting.

**Do not reimplement the pacing.** It lives in exactly one place — the
`flick-core` crate — and every client, including this one, is a consumer of it.
If you need pacing for text that is not in a library, call the `preview_timeline`
tool on the MCP server rather than writing your own weights.

`GET /api/books/{id}/text` returns the same tokens grouped into paragraphs.
Flattened across paragraphs, its word indices match the timeline exactly, so
you can map between "what the reader is looking at" and "where that is in the
document" without heuristics.

## Getting content in

| You have | Call |
|---|---|
| Text in hand | `POST /api/books` with `{"text": "...", "title": "..."}` |
| A file (PDF, EPUB, txt, Markdown, Kindle clippings) | `POST /api/books`, multipart, field `file` |
| A URL | `POST /api/import/url` with `{"url": "..."}` |
| Page HTML you already captured | `POST /api/import/html` |
| Nothing, but you want something to read | `GET /api/catalog`, then `POST /api/catalog/{slug}/add` |

Uploads are sniffed by content, not by extension: `%PDF` is a PDF, a zip
carrying an EPUB mimetype is an EPUB, UTF-8 text with `==========` separators
is a Kindle "My Clippings" export, and other UTF-8 text is plain text.
Anything else is a `400`. The cap is 25 MB.

`POST /api/import/url` fetches on your behalf with a 20-second timeout, a 25 MB
cap and at most five redirects, and extracts either the raw file or the
readable article text. It is SSRF-guarded — http(s) only, and every resolved
address must be public unicast, re-checked on each redirect hop. Pointing it at
private infrastructure will fail by design.

## Searching

`GET /api/books?q=` full-text searches titles and stored text through SQLite
FTS5 and returns matching books. Omit `q` for the whole library. A malformed
query is a `400`, never a 500.

## Reading position and stats

`PUT /api/books/{id}/position` saves where the reader got to, and is also how
words-read are reported:

```json
{"position": 812, "read": 240, "day": "2026-08-08"}
```

`read` is words consumed since your last report, clamped server-side to 0–500
per report. `day` is the **client's local date**, because a streak is a
human-day concept; the server rejects days more than two days from its own to
stop clock abuse.

`GET /api/stats` returns today's words, the all-time total, the daily goal
(300), the current and best streak, and the last 42 days. Streak celebrations
are client-driven — the API never pushes.

## Rate limits and etiquette

Fixed-window per-client limits sit on the abuse-prone endpoints
(`/api/auth/login`, `/api/auth/guest`, `/api/import/url` and friends).
Everything else is unlimited. A `429` carries `Retry-After`; honour it rather
than retrying immediately.

Two things worth asking of an automated client:

1. **Mint one guest session and keep it.** A new session per request creates a
   new user row, seeded with the whole starter library, every time.
2. **Don't use `/api/import/url` as a general-purpose fetcher.** It exists to
   put an article into someone's library.

## What is not here

There is no bearer-token flow, no OAuth authorization server and no API-key
issuance. The credential is a first-party session cookie, and the reason it
works this way is that flick is guest-first: there is nothing to gate, so there
is no gate. [`/auth.md`]({{BASE}}/auth.md) covers this in full.

The `/api/admin/*` surface is the operator break-glass panel. It is
deliberately not an agent-facing API and is not described in `openapi.json`.
