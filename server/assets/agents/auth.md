# auth.md — how an agent authenticates to flick

flick is guest-first for people, and guest-first for agents too. There is no
registration gate, no API-key issuance queue and no client-credentials dance:
an agent mints its own anonymous identity in one unauthenticated request and
starts working.

This document is served at `{{BASE}}/auth.md` and describes the flick server
you fetched it from. Self-hosted flick servers serve their own copy, with
their own base URL, and behave identically.

## The short version

```http
POST {{BASE}}/api/auth/guest
Content-Type: application/json

{}
```

The `201` response sets a `flick_session` cookie and returns the user row.
Send that cookie on every subsequent `/api/*` request. That is the whole
protocol.

```sh
curl -sS -c jar.txt -X POST {{BASE}}/api/auth/guest
curl -sS -b jar.txt  {{BASE}}/api/books
```

## Audience

This document is for autonomous agents and machine clients that want to read,
search or build a reading library through the flick HTTP API, and for MCP
clients connecting to `{{BASE}}/mcp`. Human sign-in uses the same endpoints
behind the app's own UI.

## Registration and provisioning endpoints

| Purpose | Endpoint | Credentials required |
|---|---|---|
| Provision an anonymous identity | `POST {{BASE}}/api/auth/guest` | none |
| Claim that identity as an account | `POST {{BASE}}/api/auth/register` | email + password, sent **with the guest cookie still attached** |
| Sign in to an existing account | `POST {{BASE}}/api/auth/login` | email + password |
| Discover which methods an email has | `POST {{BASE}}/api/auth/lookup` | none |
| Federated sign-in, when configured | `GET {{BASE}}/api/auth/oauth/{provider}/login` | browser redirect |
| End the session | `POST {{BASE}}/api/auth/logout` | the session cookie |

`GET {{BASE}}/api/auth/providers` lists the federated providers this
particular server has credentials for. On a server with none configured, the
list is empty and password or email-code sign-in is the only account path —
which does not affect the anonymous flow above at all.

## Supported methods

- **anonymous** — `POST /api/auth/guest`. No credentials, no email, no rate of
  approval. This is the method agents should use.
- **password** — `POST /api/auth/register` / `POST /api/auth/login`, argon2id.
- **email code** — `POST /api/auth/code/request` then
  `/api/auth/code/verify`, a 6-digit code valid 10 minutes, 5 attempts,
  single use. Existing accounts only.
- **federated (OIDC / OAuth 2.0)** — Google, GitHub, or any generic OIDC
  issuer, when the operator has configured one. This is a *browser* login flow
  that ends in a flick session cookie; the identity provider's own tokens are
  never accepted as flick API credentials.

## How the credential is used

The credential is a first-party session cookie:

```
flick_session=<opaque token>; HttpOnly; SameSite=Lax; Secure
```

`Secure` is set whenever the server's public URL is https. Because the cookie
is `SameSite=Lax`, a cross-site `POST` never carries it — browser-embedded
agents must operate same-origin, and out-of-browser agents should send the
cookie explicitly.

flick issues no bearer tokens for user-facing API calls, so there is no
`Authorization: Bearer` path, no refresh token and no token introspection
endpoint. That is why this server publishes **no**
`/.well-known/oauth-authorization-server` and no
`/.well-known/oauth-protected-resource`: it is not an OAuth authorization
server, and advertising metadata for token flows it does not implement would
send agents down a path that cannot work. The one bearer token in the system
is the operator break-glass credential for `/api/admin/*`, which is
deliberately not an agent-facing surface.

## Claiming an anonymous identity

An anonymous guest is a real user row, not a scratch buffer: it owns books,
reading positions, settings and stats, and they persist. If a human later
wants to keep that work, register or sign in **while still sending the guest
cookie** — the guest's library and stats merge into the target account and the
guest row is deleted. On an id collision the existing account's data wins, and
for a work present on both sides the copy with the further reading position
survives.

## Limits an agent should expect

- Per-client fixed-window rate limits on the abuse-prone endpoints —
  `POST /api/auth/login`, `POST /api/auth/guest` and `POST /api/import/url`
  among them. A `429` carries `Retry-After`; honour it.
- Uploads are capped at 25 MB.
- On the hosted edition, the free plan allows 15 user-sourced uploads per ISO
  week. Self-hosted servers enforce nothing.
- `POST /api/import/url` fetches on your behalf under an SSRF guard: http(s)
  only, and every resolved address must be public unicast, re-checked on each
  redirect hop. Do not use it to reach private infrastructure; it will refuse.

## Machine-readable summary

```yaml
agent_auth:
  skill: {{BASE}}/.well-known/agent-skills/flick-api/SKILL.md
  register_uri: {{BASE}}/api/auth/guest
  anonymous:
    method: anonymous
    identity_types_supported: ["anonymous"]
    credential_types_supported: ["cookie"]
    credential_name: flick_session
    register_uri: {{BASE}}/api/auth/guest
    claim_uri: {{BASE}}/api/auth/register
    scopes_supported: ["library:read", "library:write", "stats:read"]
    documentation_uri: {{BASE}}/auth.md
  password:
    method: password
    identity_types_supported: ["email"]
    credential_types_supported: ["cookie"]
    credential_name: flick_session
    register_uri: {{BASE}}/api/auth/register
    login_uri: {{BASE}}/api/auth/login
    revocation_uri: {{BASE}}/api/auth/logout
    documentation_uri: {{BASE}}/auth.md
```

The scope names above describe what the session can reach; flick does not ask
for them at provisioning time and does not issue scoped credentials. A session
can do everything its own user can do, and nothing any other user can.

## See also

- API description: [`{{BASE}}/openapi.json`]({{BASE}}/openapi.json)
- API documentation: [`{{BASE}}/docs/api`]({{BASE}}/docs/api)
- API catalog: [`{{BASE}}/.well-known/api-catalog`]({{BASE}}/.well-known/api-catalog)
- MCP server card: [`{{BASE}}/.well-known/mcp/server-card.json`]({{BASE}}/.well-known/mcp/server-card.json)
- Plain-language overview: [`{{BASE}}/llms.txt`]({{BASE}}/llms.txt)
- The binding specification: <https://github.com/one-more-refactor/flick/blob/master/docs/CONTRACTS.md>
