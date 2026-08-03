## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-17 - [Authentication Input Length Validation]
**Vulnerability:** Lack of input length constraints on password, email, and display name fields across registration, login, profile update, and admin login endpoints left the application vulnerable to CPU exhaustion DoS (due to expensive Argon2id hashing of extremely long strings) and potential database/memory bloating.
**Learning:** Security controls like password hashing (Argon2id) can be turned into Denial-of-Service vectors if the payload lengths are not bounded before initiating cryptographic operations or database lookups.
**Prevention:** Enforce strict length limits (e.g., maximum 128 characters for passwords, 254 characters for email addresses, and 100 characters for display names) on all incoming authentication payloads, and immediately short-circuit with rejection before processing further.
