# flick-backend

[![ci](https://github.com/one-more-refactor/flick-backend/actions/workflows/ci.yml/badge.svg)](https://github.com/one-more-refactor/flick-backend/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/one-more-refactor/flick-backend?labelColor=111111&color=f2ede5)](https://github.com/one-more-refactor/flick-backend/releases/latest)
[![commits since](https://img.shields.io/github/commits-since/one-more-refactor/flick-backend/latest?labelColor=111111&color=f2ede5)](https://github.com/one-more-refactor/flick-backend/compare)
[![license](https://img.shields.io/badge/license-AGPL--3.0-f2ede5?labelColor=111111)](LICENSE)

The Rust backend for [**flick**](https://github.com/one-more-refactor/flick): the reading engine and the API server. One contract — [`CONTRACTS.md`](https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md) — every client talks to this API and nothing else.

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

**`core/`** (`flick-core`) — the engine. Pure, deterministic, no I/O. It doesn't just flash words, it paces them: ORP pivot alignment, Zipf-frequency weighting (rare words get more time), long-word chunking, wrap-up pauses at clause and sentence ends.

**`server/`** (`flick-server`) — axum + SQLite (bundled rusqlite, WAL, versioned migrations — no external DB). Guest sessions, argon2id passwords, email login codes, OIDC/OAuth; a guest's library **merges** into their new account on signup. Brotli/gzip response compression. IPs pseudonymised before storage; GDPR delete + export are first-class endpoints.

## Data model

One SQLite file (WAL, bundled rusqlite, `PRAGMA user_version` migrations — v10 today). FK cascades make account deletion total:

```mermaid
erDiagram
    users ||--o{ books : owns
    users ||--o{ reading_days : "streak / goal"
    users ||--o{ sessions_log : "per-read stats"
    users ||--o{ identities : "oauth / oidc"
    users ||--o{ friends : ""
    users ||--o{ admin_sessions : "panel logins (v10)"
    books ||--o| books_fts : "FTS5"

    users {
        text id PK
        text email "null for guests"
        text password_hash "argon2id"
        int  guest
        int  is_admin
        text plan
    }
    books {
        text id PK
        text user_id FK
        text title
        text text "stored source"
        int  position
        int  deleted_at "soft trash"
    }
```

## Auth, in one picture

Readers use cookie sessions; the admin panel is bearer-only on its own origin:

```mermaid
sequenceDiagram
    participant W as flick-web
    participant A as flick-admin
    participant S as flick-server

    W->>S: POST /api/auth/guest
    S-->>W: Set-Cookie flick_session (guest)
    W->>S: POST /api/auth/register (same session)
    Note over S: merge_guest_into — library + progress follow
    S-->>W: Set-Cookie flick_session (account)

    A->>S: POST /api/admin/login {email, password}
    Note over S: is_admin only · argon2 verify<br/>with dummy-hash timing defense
    S-->>A: bearer token (sha256-stored, 12 h)
    A->>S: GET /api/admin/overview · Authorization: Bearer …
```

## Run it

```sh
cargo run --release -p flick-server              # from source
podman build -t flick-backend -f deploy/Containerfile .   # prod image (web client baked in)
```

Self-hosting everything is one command — see [**flick › Self-hosting**](https://github.com/one-more-refactor/flick#self-hosting).

## Configuration

Environment variables (full list in [`CONTRACTS.md`](https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md)):

| Variable | Default | Purpose |
|---|---|---|
| `FLICK_EDITION` | `selfhost` | `selfhost` (all free) or `hosted` |
| `FLICK_ADDR` | `0.0.0.0:8484` | bind address |
| `FLICK_DATA_DIR` | `./data` | SQLite lives here |
| `FLICK_PUBLIC_URL` | `http://localhost:8484` | canonical URL |
| `FLICK_WEB_DIST` | auto | built web client, served at `/` |
| `FLICK_SMTP_URL` | — | login-code email; unset = codes logged |
| `FLICK_OIDC_*`, `FLICK_OAUTH_*` | — | SSO providers |
| `FLICK_ADMIN_TOKEN` | — | enables admin endpoints |

## Verify

```sh
cargo test --workspace && cargo clippy --all-targets -- -D warnings
```

Releases: bump the crate versions, tag `vX.Y.Z`, push the tag — CI verifies tag = manifest, runs the suite, and publishes the release.

## License

[AGPL-3.0-only](LICENSE). Run a modified network service → offer users its source (§13).
