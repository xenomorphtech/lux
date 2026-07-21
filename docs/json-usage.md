# JSON Usage

Lux now has a first-class `Dynamic` boundary for JSON and other runtime-shaped
data. The JSON wrappers are still thin, but you no longer have to reach OTP
JSON through raw `apply_module(:json, ...)` calls.

## Working Example

See [examples/json.lux](/home/sdancer/lux/examples/json.lux).

Core shape:

```lux
fn main() -> Bool {
    let decoded: Dynamic =
        dynamic_json_decode("{\"name\":\"Lux\",\"ok\":true,\"count\":3,\"items\":[1,2,3]}")
    let encoded =
        dynamic_json_encode(dynamic(%{"status" => "ok", "mode" => "demo"}))

    match decode_field(decoded, "name", |value| dynamic_string_result(value)) {
        DynamicResult::Ok(name) =>
            name == "Lux" && str_contains(encoded, "\"status\":\"ok\""),
        DynamicResult::Err(
            DecodeError::At(
                DecodePathItem::Field(field),
                DecodeError::Expected(kind)
            )
        ) => field == "name" && kind == "string",
        DynamicResult::Err(_) => false
    }
}
```

For numeric-to-string fallback inside a decoder, use a real Lux string conversion:

```lux
decode_map(dynamic_int_result(value), |n| str_from_chars(integer_to_list(n)))
```

This works because:
- Lux strings are binaries.
- OTP `json:decode/1` returns binary strings and maps with binary keys.
- `dynamic_json_decode` returns `Dynamic`.
- `dynamic_get`, `dynamic_at`, and `dynamic_*` extractors let you inspect that value.
- `dynamic_json_encode` handles the OTP `iolist()` return internally.
- `dynamic_*_result` plus `decode_field` / `decode_optional_field` / `decode_field_or` / `decode_index` / `decode_list` / `decode_optional` / `decode_dict` / `decode_then` give you a typed, non-throwing decode path.
- failures are structured as `DecodeError`, not opaque strings

## Current JSON Model In Lux

What works well today:
- decoding JSON into `Dynamic`
- reading object fields with `dynamic_get`
- reading arrays with `dynamic_at`
- checking dynamic shape with `dynamic_is_*`
- runtime-checked extraction with `dynamic_string`, `dynamic_int`, `dynamic_bool`, `dynamic_list`, `dynamic_map`
- typed result-returning extraction with `dynamic_*_result`
- basic composition with `decode_map`, `decode_then`, `decode_field`, `decode_optional_field`, `decode_field_or`, `decode_index`, `decode_list`, `decode_optional`, `decode_dict`, and `decode_one_of`
- path-aware decode failures via `DecodeError::At(...)`
- encoding homogeneous arrays and homogeneous object maps

What is awkward today:
- there is still no direct `json:decode(...)` source syntax
- JSON objects and arrays are naturally heterogeneous, while Lux maps and lists are homogeneous in the type system
- there is no dedicated JSON null value beyond the atom `null`
- higher-order decoder coverage is still small

## What Is Missing For Practical JSON Work

The minimum useful additions are:

1. wider `one_of` / collection combinators
2. a dedicated `Json` ADT for code that wants static structure instead of `Dynamic`
3. decode/encode option surfaces from OTP where they matter
4. builders for heterogeneous dynamic objects and arrays

So the current state is better than raw `Any`, but still below a full Gleam-style decoder layer.
