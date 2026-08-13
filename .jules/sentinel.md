## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-17 - [Argon2id CPU Exhaustion DoS Prevention]
**Vulnerability:** Hashing endpoints (registration, login, and admin login) did not enforce safe length boundaries on the incoming password strings prior to Argon2id hashing or verification, allowing a remote attacker to exhaust server CPU cycles via excessively long strings.
**Learning:** Heavy cryptographic processes like Argon2id are vulnerable to Denial of Service (DoS) when raw inputs are unconstrained on *all* endpoints—including login, not just registration—as verifying a large password on a blocking thread can still block worker threads.
**Prevention:** Enforce tight, early-exit validation constraints (e.g., at most 128 characters) on all password-receiving endpoints before calling Argon2id functions, and fail securely without introducing timing anomalies.
