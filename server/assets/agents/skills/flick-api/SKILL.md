---
name: flick-api
description: Use the flick HTTP API to build and read a speed-reading library — mint an anonymous session, import PDFs/EPUBs/URLs/text, search full-text, fetch paced RSVP timelines, and track reading position and streaks. Use when working with flick, myflick.app, or any self-hosted flick server.
license: AGPL-3.0-only
---

# Using the flick API

flick turns text into a **paced reading timeline**: one word at a time, each
anchored on its pivot letter, each with its own duration. This skill covers
getting a session, getting content in, and getting timelines out.

Machine-readable description: `{{BASE}}/openapi.json`.
Base path for everything below: `{{BASE}}/api`.

## Authenticate — one request, no registration

flick is guest-first. There is no API key to request and no OAuth flow to
complete.

```sh
curl -sS -c jar.txt -X POST {{BASE}}/api/auth/guest
```

The `201` sets a `flick_session` HttpOnly cookie. Send it on every subsequent
`/api/*` call. Keep the jar for the life of your task — a fresh session every
request creates a fresh user row each time, which is wasteful and pollutes the
server's user table.

A guest is a real user: it owns books, positions, settings and stats, and they
persist. If a human later wants to keep the work, they register **while the
guest cookie is still attached** and everything merges into their account.

## Get content in

```sh
# text you already have
curl -sS -b jar.txt -X POST {{BASE}}/api/books \
  -H 'content-type: application/json' \
  -d '{"title":"Notes","text":"The quick brown fox…"}'

# a file — PDF, EPUB, .txt, Markdown, Kindle "My Clippings"
curl -sS -b jar.txt -X POST {{BASE}}/api/books -F file=@paper.pdf

# a web page or a hosted file
curl -sS -b jar.txt -X POST {{BASE}}/api/import/url \
  -H 'content-type: application/json' -d '{"url":"https://example.com/essay"}'
```

Uploads are sniffed by content, not extension. The cap is 25 MB. `import/url`
is SSRF-guarded and will refuse private addresses — it is for putting articles
into a library, not for general fetching.

Every new user, guests included, is already seeded with the public-domain
catalog (`GET {{BASE}}/api/catalog`), so there is something to read before you
import anything.

## Read

```sh
curl -sS -b jar.txt "{{BASE}}/api/books"           # the library
curl -sS -b jar.txt "{{BASE}}/api/books?q=fovea"   # full-text search
curl -sS -b jar.txt "{{BASE}}/api/books/$ID/timeline"
curl -sS -b jar.txt "{{BASE}}/api/books/$ID/text"
```

`/timeline` is the paced stream:

```json
{"version":1,"word_count":1841,
 "words":[["Whenever",2,1.14],["I",0,0.55],["find",1,0.92]]}
```

Each entry is `[text, orp_index, weight]`. Display the token for
`weight * (60000 / wpm)` milliseconds, with the character at `orp_index`
highlighted and held in a fixed screen position.

`/text` returns the same tokens grouped into paragraphs. Flattened, its indices
match the timeline exactly — so a `position` of 812 in the timeline is word 812
in the flattened text. **If you want to read or summarise the content, use
`/text`,** not the timeline.

## Save position and report reading

```sh
curl -sS -b jar.txt -X PUT {{BASE}}/api/books/$ID/position \
  -H 'content-type: application/json' \
  -d '{"position":812,"read":240,"day":"2026-08-08"}'
```

`read` is words consumed since your last report (clamped to 0–500 per report).
`day` is the reader's **local** date. `GET {{BASE}}/api/stats` returns totals,
the 300-word daily goal, and the streak.

## Rules that will save you a debugging session

- **Never reimplement the pacing.** Weights come from `flick-core` and nowhere
  else. For text that is not in a library, call the `preview_timeline` tool on
  the MCP server at `{{BASE}}/mcp` instead of inventing weights.
- **`orp_index` is a character index into `text`**, and `text` may be a chunk
  of a long word rather than the whole word — long words are split for display.
- **Honour `429` + `Retry-After`.** Login, guest creation and URL import are
  rate-limited per client.
- **A `409` from `POST /api/catalog/{slug}/add` is success.** The book is
  already in the library; the error body carries `book_id`, and clients open
  that copy.
- **`404` on a book means "not yours or not there"** — the two are deliberately
  indistinguishable. Do not retry.
- Self-hosted servers run the identical API. Swap the base URL and nothing else
  changes; `GET /api/meta` tells you the edition and version you are talking to.

## Full documentation

- Prose: `{{BASE}}/docs/api`
- Authentication in depth: `{{BASE}}/auth.md`
- The binding specification:
  <https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md>
