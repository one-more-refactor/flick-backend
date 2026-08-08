---
name: self-host-flick
description: Deploy and operate a self-hosted flick speed-reading server — Compose or from source, environment configuration, reverse proxy and cookie/rate-limit gotchas, OIDC/OAuth single sign-on, SMTP, backups and upgrades. Use when installing, configuring, proxying or troubleshooting a flick instance.
license: AGPL-3.0-only
---

# Self-hosting flick

One container, one SQLite file. No Redis, no Postgres, no external services.
Everything is free in the self-host edition — there is nothing to license and
nothing to unlock.

## Install

```sh
curl -fsSL https://myflick.app/install.sh | sh     # → http://localhost:8484
```

That is Compose underneath; the equivalent explicit form is a
`docker-compose.yml` that builds `deploy/Containerfile` from the
`flick-backend` repo (which pulls in the web client) and keeps the library in a
named `flick-data` volume. Podman works the same way, and the ready-made
Quadlet units live in the backend repo's `deploy/`.

From source, if you would rather not run a container: build the Svelte client
with Bun, then the Rust server, and point one at the other.

```sh
git clone https://github.com/one-more-refactor/flick-web.git
cd flick-web && bun install && bun run build && cd ..

git clone https://github.com/one-more-refactor/flick-backend.git
cd flick-backend
FLICK_WEB_DIST="$PWD/../flick-web/dist" cargo run --release -p flick-server
```

`target/release/flick-server` is the entire server. It creates its data
directory and `flick.db` on first start.

## Configure

Everything is a `FLICK_*` environment variable and everything is optional.

| Var | Meaning | Default |
|---|---|---|
| `FLICK_EDITION` | `selfhost` or `hosted`. Leave it alone. | `selfhost` |
| `FLICK_ADDR` | Listen address | `0.0.0.0:8484` |
| `FLICK_DATA_DIR` | SQLite database + storage | `./data` |
| `FLICK_PUBLIC_URL` | External base URL | `http://localhost:8484` |
| `FLICK_WEB_DIST` | Built web client directory | `./web/dist`, then `../web/dist` |
| `FLICK_OIDC_ISSUER` / `_CLIENT_ID` / `_CLIENT_SECRET` | Generic OIDC SSO | — |
| `FLICK_OIDC_NAME` | Label on the SSO button | `SSO` |
| `FLICK_OAUTH_GOOGLE_CLIENT_ID` / `_SECRET` | Google sign-in | — |
| `FLICK_OAUTH_GITHUB_CLIENT_ID` / `_SECRET` | GitHub sign-in | — |
| `FLICK_SMTP_URL` | `smtp[s]://user:pass@host:port` for login codes | — |
| `FLICK_SMTP_FROM` | From address | `flick <no-reply@localhost>` |
| `FLICK_ADMIN_TOKEN` | Break-glass bearer for the operator panel | — |

Credential pairs only take effect when **both** halves are set, and blank
values count as unset.

## The three things that actually go wrong

1. **Cookies don't stick behind a proxy.** Set `FLICK_PUBLIC_URL` to the
   `https://` URL. That is what flips the session cookie's `Secure` attribute
   on — and it is also what OAuth/OIDC redirect URIs are built from, so an
   unset or wrong value breaks SSO in the same stroke.

2. **Rate limiting keys on the wrong address.** The first `X-Forwarded-For`
   entry is trusted **only** when the direct peer is loopback, RFC1918, CGNAT
   (100.64/10) or `fc00::/7` — your proxy. If flick sees the proxy's public IP
   as the peer, every visitor shares one bucket. Make sure the proxy sets
   `X-Forwarded-For`.

3. **SSO redirect URI mismatch.** The callback is
   `{FLICK_PUBLIC_URL}/api/auth/oauth/{provider}/callback` — register exactly
   that at the identity provider, trailing slash and all.

## SSO

Generic OIDC (Authentik, Keycloak, Zitadel, anything conformant):

```sh
FLICK_OIDC_ISSUER=https://auth.example.com/application/o/flick/
FLICK_OIDC_CLIENT_ID=...
FLICK_OIDC_CLIENT_SECRET=...
FLICK_OIDC_NAME=Authentik     # the button reads "Continue with Authentik"
```

Google and GitHub take their own client id/secret pairs. Each provider becomes
a login button as soon as its credentials are present;
`GET /api/auth/providers` reports which ones are live.

Account linking is by **verified** email: signing in through a provider with an
email that matches an existing account links to it rather than creating a
duplicate. Providers whose email claim is unverified do not link.

Note that flick is an OIDC *client*, never an authorization server. It exchanges
a successful federated login for its own first-party session cookie; the
identity provider's tokens are not accepted as flick API credentials.

## Mail

`FLICK_SMTP_URL` unset means 6-digit login codes are written to the server log
at `info` level. That is fine for a personal instance and wrong for a shared
one — anyone who can read the journal can sign in as anyone.

## Reverse proxy

flick serves plain HTTP. Any TLS terminator in front works:

```
flick.example.com {
    reverse_proxy 127.0.0.1:8484
}
```

Unknown non-`/api` GET paths serve `index.html` so client-side routes
deep-link correctly — do not add your own SPA rewrite rules on top.

## Backups

The database is one SQLite file in `FLICK_DATA_DIR`, in WAL mode. Do not copy
it while the server runs; take a consistent snapshot instead:

```sh
sqlite3 /var/lib/flick/data/flick.db ".backup '/backups/flick-$(date +%F).db'"
```

A nightly timer keeping ~14 days is the pattern the hosted instance uses.
`systemctl stop` sends plain SIGTERM, which is safe — WAL recovers cleanly on
the next start.

## Upgrades

Container installs: pull the new image and recreate. Source installs: rebuild
the client and the binary, then restart. Schema migrations run automatically at
startup, forward-only. Check the running version with:

```sh
curl -sS http://localhost:8484/api/meta
```

## Full guide

<https://github.com/one-more-refactor/flick/blob/master/docs/SELF-HOSTING.md>
