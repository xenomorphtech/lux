# Compiled Sandbox Safety in Lux

This document defines Lux's sandbox model for untrusted code.

## Security Model

Lux enforces sandbox boundaries at compile time and uses BEAM runtime controls for resource isolation.

- Compile-time capability control:
  - `external` declarations can be disabled entirely.
  - imports are checked against an allowlist.
  - the type environment is seeded only with approved built-ins.
- Runtime resource control (host responsibility):
  - run compiled code in a dedicated Erlang process.
  - apply memory limits (`max_heap_size`) and timeouts/monitoring.

## Why Compile-Time Enforcement

Runtime sandboxing hides dangerous APIs after code is loaded. Lux instead blocks dangerous constructs before code generation.

- If an API is absent from the type environment, symbol resolution fails.
- If a module import is not allowlisted, compilation fails.
- If `external` is disabled, parsing fails immediately.

This gives zero sandbox overhead in generated code for capability checks.

## Implemented Controls

### 1. FFI Gate: `external` disabled in sandbox

In sandbox mode, parser options set `allow_extern = false`.

- Any `external` block causes a parse error.
- A second session-level validation rejects `Item::Extern` as defense in depth.

### 2. Import Allowlist

In sandbox mode, top-level `use` declarations are validated against an allowlist.

Default allowed modules:

- `prelude`
- `list`
- `option`
- `result`

Any other `use` fails with `SecurityError::ImportDisallowed`.

### 3. Type Environment as Capability Matrix

`Session::register_builtins` now depends on security profile:

- `Trusted`: full built-in set (I/O, OS, process registry, file operations, etc.)
- `Sandboxed`: pure/transformational built-ins only

In sandbox mode, forbidden names (for example `whereis`, `file_read`, `os_cmd`) are not present in the environment. Calls to them fail type inference as unbound variables.

## Profiles

`SessionConfig` supports two profiles:

- `SecurityProfile::Trusted`
- `SecurityProfile::Sandboxed`

Helpers:

- `SessionConfig::trusted()`
- `SessionConfig::sandboxed_default()`

## CLI Usage

The compiler now supports:

- `--sandbox`: compile with sandbox restrictions

Examples:

```bash
lux --sandbox examples/fib.lux
lux --sandbox --emit-core script.lux
```

## BEAM Runtime Hardening (Recommended)

Compile-time controls do not solve non-termination or unbounded allocation. Run sandboxed output with BEAM limits:

- spawn isolated worker process
- monitor worker and enforce wall-clock timeout
- configure `max_heap_size`
- avoid exposing global registry access (`whereis`-style patterns)
- pass explicit capabilities (PIDs/refs/tokens) into entrypoints

## Current Test Coverage

Added tests verify:

- `external` rejected in sandbox parser path
- disallowed `use` import rejected by security policy
- dangerous built-in unavailable in sandbox type environment

These tests live in:

- `src/syntax/parser.rs`
- `src/driver/session.rs`
