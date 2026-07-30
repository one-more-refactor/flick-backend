## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-07-16 - [Enforcing Input Validation Limits to Prevent DoS]
**Vulnerability:** Registration, login, and profile update endpoints did not enforce maximum length bounds on user inputs such as passwords, emails, and display names. Computational processes like Argon2id password hashing are resource-intensive; verifying or hashing extremely long passwords (e.g., thousands of characters) could lead to CPU exhaustion, causing a Denial of Service (DoS).
**Learning:** Cryptographic operations are highly sensitive to input length. Without strict length constraints prior to processing, attackers can easily trigger resource starvation.
**Prevention:** Always sanitize and validate the length of user inputs (e.g., email max 254 chars, password max 128 chars, name max 100 chars) as early as possible on the endpoint handlers before hitting database or cryptographic primitives.
