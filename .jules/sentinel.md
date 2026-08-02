## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-17 - [Argon2id CPU/Memory Exhaustion Denial of Service Prevention]
**Vulnerability:** The registration and login endpoints lacked upper-bound limits on the password string length. This allowed any unauthenticated user to send extremely large password strings (up to the request body size limit of 10 MB or more), forcing the CPU-intensive Argon2id hashing algorithm to perform excessive computations and consume memory, leading to a severe Denial of Service (DoS) vulnerability.
**Learning:** CPU/memory-hard hashing functions like Argon2id are highly susceptible to resource exhaustion attacks if inputs are not strictly validated before hashing.
**Prevention:** Strictly enforce a maximum password length limit (e.g., 128 characters) on all login, registration, and credential update paths before invoking the hashing/verification process.
