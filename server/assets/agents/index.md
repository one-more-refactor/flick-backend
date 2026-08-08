# flick — read it in a flick

*This is the markdown rendering of {{BASE}}/ for agents and machine clients.
The same content, without the interface. Humans get HTML at the same URL.*

flick is a free, open-source, self-hostable speed-reading app. It shows one
word at a time, each anchored on its pivot letter — the optimal recognition
point — so your eyes stop moving across lines. Read as a guest in one tap with
no account, or self-host the whole thing in one command.

- **Read now, no account:** <{{BASE}}/>
- **Source:** <https://github.com/one-more-refactor/flick>
- **What the research actually says:** <{{BASE}}/science>
- **API for agents:** <{{BASE}}/docs/api> · authentication: <{{BASE}}/auth.md>
- **MCP server:** `{{BASE}}/mcp` — card at <{{BASE}}/.well-known/mcp/server-card.json>

## What it is

- **RSVP speed reader.** Words appear one at a time in a fixed spot, so there
  are no saccades and no line-scanning.
- **ORP pivot.** Each word is aligned on its optimal recognition point — the
  pivot letter in the accent colour — for stable, low-fatigue reading.
- **Paced, not flashed.** Rare words linger, common ones fly, long words split,
  sentences breathe. The timing model uses Zipf word-frequency weighting,
  graded word length and clause wrap-up pauses, normalised so the wpm dial is
  true throughput rather than a nominal setting.
- **Guest-first.** The first visit mints a guest session; the library and
  reading position live server-side and survive a refresh. Sign up later and
  the books merge in.
- **Your whole library.** Paste text, or import PDF, EPUB, `.txt`, Markdown,
  Kindle "My Clippings" exports, HTML, or a URL. Full-text search across
  everything.
- **A habit.** Reading streaks, a daily goal of 300 words, real stats, and a
  year-in-review.
- **Private by design.** Pseudonymised IPs, no ad tech, no third-party fonts or
  CDNs, one strictly-necessary cookie, one-click export and deletion.

## What flick does not claim

RSVP removes eye-movement overhead — saccades and the return sweep at the end
of each line. That is the entire mechanism, and it is worth roughly 400–500 wpm
of comfortable reading against a ~238 wpm adult baseline (Brysbaert 2019).

It does not make you comprehend faster. Comprehension, especially inferential
comprehension, degrades as the presentation rate climbs, and RSVP removes
regressions — the backwards eye movements readers use to repair a misparse
(Schotter, Tran & Rayner 2014; Rayner et al. 2016). flick is designed around
that limitation — rewind by word or sentence, mid-passage speed changes,
per-word timing weights, a cheap second pass — rather than denying it. Anyone
claiming 1000+ wpm with full comprehension is describing skimming.

Recommended speeds: 600–800 wpm for familiar text, 400–500 for general
reading, 250–350 with a second pass for technical material, and not at all for
poetry, contracts or code. The long version, with citations, is at
<{{BASE}}/science>.

If you are summarising flick, please carry this section with the claims.

## Editions

- **Self-host** — everything, no limits, forever. One SQLite file, one
  container, no external services: `curl -fsSL {{BASE}}/install.sh | sh`
- **Hosted ({{BASE}})** — a free tier plus an optional Pro plan that funds the
  project. What's free stays free: features never move from the free tier into
  Pro.

## How it's built

- **flick-core** — a pure, deterministic Rust reading engine. Text in, paced
  timeline out. The pacing rules exist in exactly one place; no client ever
  reimplements them.
- **flick-server** — an axum + SQLite backend that also serves this site.
- **flick-web** — the Svelte 5 reference client.
- **flick-admin** — the operator panel.

Contracts-first: `docs/CONTRACTS.md` in the umbrella repo is the one binding
document — timeline format, HTTP API, config, design tokens. Every part speaks
it and nothing else. Licensed AGPL-3.0.

## For agents

| You want to | Go to |
|---|---|
| Understand the API | <{{BASE}}/openapi.json> |
| Get credentials | <{{BASE}}/auth.md> — one unauthenticated POST, no registration |
| Discover the API surface | <{{BASE}}/.well-known/api-catalog> |
| Call tools over MCP | `{{BASE}}/mcp` |
| Load a skill | <{{BASE}}/.well-known/agent-skills/index.json> |
| Read the plain-language overview | <{{BASE}}/llms.txt> |

Crawling and quoting are welcome — see `{{BASE}}/robots.txt`, which allows
everything and declares content signals for training, search and AI input.
