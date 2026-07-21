# Binary Formats

Lux uses Erlang binaries as its string representation.

## Current model

- String literals like `"ABCD"` compile to binaries, not charlists.
- Binary literals use `<<...>>`.
- Integer binary segments default to little-endian.
- Unsized integer segments default to 8 bits.
- Binary pattern matching supports sized integer segments and a trailing binary capture.

That means this evaluates to `true`:

```lux
fn main() -> Bool {
    <<0x44434241:32>> == "ABCD"
}
```

The `0x44434241` segment is interpreted as a 32-bit little-endian integer, so its bytes are:

```text
41 42 43 44
```

which is `"ABCD"`.

## Examples

Binary construction:

```lux
fn greeting() -> String {
    <<0x44434241:32>>
}
```

Explicit segment formats:

```lux
fn greeting() -> String {
    <<65/utf8, "BCD"/binary>>
}
```

Binary pattern matching:

```lux
fn tail_after_a(input: String) -> String {
    match input {
        <<0x41:8, rest>> => rest,
        _ => <<"">>
    }
}
```

## Dynamic predicates

The basic dynamic predicates now include:

- `is_nil(x)`
- `is_string(x)`
- `is_binary(x)`

With current semantics:

- `is_string("ABCD")` is `true`
- `is_binary("ABCD")` is `true`
- `is_nil([])` is `true`

## Current boundary

This is the base binary layer, not full Erlang bit syntax yet.

Currently implemented:

- default little-endian integer segments
- `/integer`
- `/binary`
- `/utf8`

Not implemented yet:

- explicit endianness specifiers
- signed integer segments
- utf16/utf32 segment specifiers
- general binary tail specifiers beyond the current trailing capture case
