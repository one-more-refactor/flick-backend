## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-17 - [Auth Input Validation and CPU Exhaustion Prevention]
**Vulnerability:** Lack of length checks on user passwords during login, registration, and admin login exposed the server to CPU exhaustion Denial of Service (DoS) attacks, as expensive Argon2id hashing is executed over arbitrarily large input streams.
**Learning:** Authentication endpoints must strictly validate input bounds (such as 128 characters for passwords and 254 for emails) prior to running cryptographic operations. Furthermore, display name limits should be checked, and third-party login providers (OIDC/OAuth) should have their display names safely truncated using UTF-8 boundary checks rather than failing.
**Prevention:** Enforce input length bounds immediately upon request entry before any DB or password-hashing tasks are triggered, and use `.char_indices().nth(limit)` to safely truncate UTF-8 strings.
