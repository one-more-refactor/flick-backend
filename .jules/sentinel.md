## 2026-04-16 - [Hardening Constant-Time Token Comparisons]
**Vulnerability:** Persistent security secrets (the `FLICK_ADMIN_TOKEN` and 6-digit email login code hashes) were being verified using custom manual comparison loops (`zip` and `fold`). Custom constant-time comparison functions are susceptible to timing attacks if compilers optimize them into early-exit loops or vectorized instructions.
**Learning:** Manual timing-safe implementations in Rust can easily be optimized away by LLVM/rustc, introducing subtle timing side-channels for critical, static secret strings.
**Prevention:** Always use established, compiler-opaque cryptography/constant-time comparison libraries like `subtle` and its `ConstantTimeEq` trait instead of rolling custom loops.

## 2026-04-16 - [Unbounded Authentication Inputs and CPU Exhaustion DoS]
**Vulnerability:** Input fields for email and password on authentication and admin login endpoints were unbounded. Passing extremely large passwords to Argon2id hashing or large emails to SQLite queries could exhaust CPU and memory resources, causing Denial of Service.
**Learning:** Checking bounds on registration is insufficient if login and lookup endpoints remain unchecked, as password verification is often the most resource-intensive step.
**Prevention:** Always enforce strict upper bounds on all user input fields (e.g., maximum 1024 characters for passwords, 254 for emails, and 12 for 6-digit codes) before performing any resource-intensive operations like hashing or database lookups.
