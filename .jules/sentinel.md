## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-16 - [Enforcing Password & Input Length Limits to Prevent Hashing DoS]
**Vulnerability:** Registration, login, and profile update endpoints lacked maximum length limits on passwords, email addresses, and names. This allowed attackers to submit excessively large strings (e.g. multi-megabyte payloads), causing high CPU load during Argon2id password hashing and database storage, leading to resource exhaustion (Denial of Service).
**Learning:** Hashing algorithms like Argon2id are computationally expensive. Omitting input length validation on fields passed directly to hashing/verification libraries is a critical vector for CPU exhaustion DoS.
**Prevention:** Always validate and enforce strict maximum length limits on user-supplied passwords (e.g., max 128 characters) and other inputs before passing them to expensive cryptographic or database operations.
