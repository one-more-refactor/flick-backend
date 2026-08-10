## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-17 - [Preventing Argon2 CPU Exhaustion DoS via Password Length Caps]
**Vulnerability:** Authentication endpoints allowing arbitrary-length passwords. Attackers could submit extremely large password strings (e.g. megabytes) to login or register, forcing the server to hash/verify them with Argon2id, resulting in CPU exhaustion and Denial of Service (DoS).
**Learning:** Even with rate limiting, password-hashing functions (like Argon2id) are computationally expensive and can be leveraged for single-request DoS if password lengths are not capped prior to processing.
**Prevention:** Enforce a maximum length limit of 128 characters on all password inputs at the handler layer before hitting the hasher.
