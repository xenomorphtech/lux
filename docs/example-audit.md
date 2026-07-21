# Example Audit

This documents the recent compile audit of `examples/*.lux` and the fixes applied afterward.

## Current Status

All examples in `examples/*.lux` compile with the current Lux compiler and CLI.

Verified with:

```bash
for f in examples/*.lux; do
  cargo run --quiet -- "$f"
done
```

## What Was Fixed

### Builtin typing drift

The audit surfaced a few builtin typing mismatches:

- `print` and `println` were typed like atom-returning Erlang calls instead of Lux `Unit`
- overloaded built-ins were given distinct symbol names: `random`, `random_below`, `map_get`, `map_get_or`, `apply`, and `apply_module`
- `take`, `drop`, and `nth` were typed in the opposite argument order from their current examples and codegen

These are fixed now.

### Pipe codegen

`pipe.lux` exposed a real Core Erlang bug in pipe lowering for local content-addressed functions.

That path emitted an invalid remote `apply` form. It now lowers to a normal remote `call ... 'apply'(...)`.

### String concat after binary strings

Once strings became binaries, `++` could no longer blindly lower to Erlang list concatenation.

That operator now dispatches correctly:

- binary strings use binary concatenation
- lists still use Erlang list concatenation

### Stale examples

The example set also contained a few stale assumptions:

- map examples assumed looser heterogeneous map usage than the current `Map<K, V>` typing
- `use_example.lux` still reflected the old source-import model instead of the current live-code direction

Those examples were rewritten to match the current compiler model.

## Notes

- `binary.lux` has been updated to demonstrate binary strings and binary pattern matching
- binary strings are documented in [binary-formats.md](/home/sdancer/lux/docs/binary-formats.md)
