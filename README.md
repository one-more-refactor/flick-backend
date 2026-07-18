# flick-backend

The Rust backend for [**flick**](https://github.com/one-more-refactor/flick) — the reading engine and the API server.

- **`core/`** (`flick-core`) — the speed-reading engine. Pure, deterministic, no I/O: it turns text into a paced stream of RSVP words with Optimal-Recognition-Point (ORP) alignment. Fully unit-tested.
- **`server/`** (`flick-server`) — an [axum](https://github.com/tokio-rs/axum) HTTP server. Exposes the JSON API under `/api`, persists to SQLite, and (optionally) serves the built web client at `/`.

It speaks one contract: [`docs/CONTRACTS.md`](https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md) in the umbrella repo. Every client — the [web app](https://github.com/one-more-refactor/flick-web), a future browser extension — talks to this API and nothing else.

```
┌──────────────┐   HTTP/JSON (/api)   ┌───────────────────────────┐
│  any client  │ ───────────────────▶ │        flick-server        │
│ web · ext …  │ ◀─────────────────── │  axum · sessions · SQLite  │
└──────────────┘   cookie session     │            │               │
                                       │            ▼               │
                                       │        flick-core          │
                                       │  RSVP · ORP · pacing model │
                                       └───────────────────────────┘
```

## The engine (`flick-core`)

flick doesn't just flash words — it paces them. Each word's dwell time is derived from a research-grounded model:

- **ORP alignment** — every word is split at its Optimal Recognition Point (the pivot your eye should land on) and rendered so that pivot stays fixed. No saccades, no line-tracking.
- **Frequency weighting** — rarer words (low [Zipf](https://en.wikipedia.org/wiki/Zipf%27s_law) frequency) get more time; common ones flick by.
- **Length grading & long-word splitting** — long tokens are chunked; each chunk earns its own dwell.
- **Wrap-up pauses** — sentence- and clause-final punctuation gets a beat, so meaning lands.

The engine is pure and deterministic, which means it's testable: `cargo test -p flick-core`.

## The server (`flick-server`)

- **SQLite** via `rusqlite` (bundled, WAL) — schema migrations are versioned with `PRAGMA user_version`. No external database.
- **Auth** — guest sessions (no signup), argon2id passwords, 6-digit email login codes, and OIDC / OAuth (Google, GitHub). Cookie sessions. On sign-up, a guest's library and progress **merge** into the new account.
- **Privacy by default** — client IPs are pseudonymised before they touch storage; account deletion (GDPR Art. 17) and data export (Art. 15/20) are first-class endpoints.
- **Editions** — `FLICK_EDITION=selfhost` (everything free, nothing metered) or `hosted` (Free / Pro).

### Run it

```sh
# from source
cargo run --release -p flick-server

# or the production image (backend + web client baked in)
podman build -t flick-backend -f deploy/Containerfile .   # run from a checkout that has web/ — see the umbrella repo
```

Self-hosting the whole thing (backend + web) is one command — see [**flick › Self-hosting**](https://github.com/one-more-refactor/flick#self-hosting).

### Configuration

All config is environment variables (full list in [`CONTRACTS.md`](https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md)):

| Variable | Default | Purpose |
|---|---|---|
| `FLICK_EDITION` | `selfhost` | `selfhost` or `hosted` |
| `FLICK_ADDR` | `0.0.0.0:8484` | bind address |
| `FLICK_DATA_DIR` | `./data` | SQLite lives here |
| `FLICK_PUBLIC_URL` | `http://localhost:8484` | canonical URL (OAuth redirects, share links) |
| `FLICK_WEB_DIST` | auto | path to the built web client to serve at `/` |
| `FLICK_SMTP_URL` | — | SMTP for login-code email; if unset, codes are logged (dev) |
| `FLICK_OIDC_*`, `FLICK_OAUTH_*` | — | SSO providers |
| `FLICK_ADMIN_TOKEN` | — | enables admin endpoints |

## Layout

```
core/          flick-core — the reading engine (lib, no I/O)
server/        flick-server — axum API + static web serving
deploy/        Containerfile + Quadlet units for rootless podman
Cargo.toml     workspace
```

## Tests

```sh
cargo test          # core + server
cargo clippy --all-targets -- -D warnings
```

## License

[AGPL-3.0-only](LICENSE). If you run a modified version as a network service, you must offer users its source (§13).
