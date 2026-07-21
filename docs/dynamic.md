# Dynamic

`Dynamic` is Lux's explicit runtime data boundary.

It is distinct from `Any`.

- `Any` is the loose FFI escape hatch.
- `Dynamic` is for data whose runtime shape is not statically known yet, but is
  expected to be inspected and decoded inside Lux.

## Current Builtins

Construction and inspection:
- `dynamic(x) -> Dynamic`
- `dynamic_typeof(x) -> Atom`
- `dynamic_is_null(x) -> Bool`
- `dynamic_is_bool(x) -> Bool`
- `dynamic_is_int(x) -> Bool`
- `dynamic_is_float(x) -> Bool`
- `dynamic_is_atom(x) -> Bool`
- `dynamic_is_string(x) -> Bool`
- `dynamic_is_binary(x) -> Bool`
- `dynamic_is_list(x) -> Bool`
- `dynamic_is_map(x) -> Bool`

Navigation:
- `dynamic_get(x, key) -> Dynamic`
- `dynamic_get_or(x, key, fallback) -> Dynamic`
- `dynamic_at(x, index) -> Dynamic`

Runtime-checked extraction:
- `dynamic_string(x) -> String`
- `dynamic_binary(x) -> String`
- `dynamic_int(x) -> Int`
- `dynamic_float(x) -> Float`
- `dynamic_bool(x) -> Bool`
- `dynamic_atom(x) -> Atom`
- `dynamic_list(x) -> List[Dynamic]`
- `dynamic_map(x) -> Map[Any, Dynamic]`

JSON wrappers:
- `dynamic_json_decode(json) -> Dynamic`
- `dynamic_json_encode(value) -> String`

Result-returning decoders:
- `dynamic_json_decode_result(json) -> DynamicResult<Dynamic>`
- `dynamic_get_result(x, key) -> DynamicResult<Dynamic>`
- `dynamic_at_result(x, index) -> DynamicResult<Dynamic>`
- `dynamic_string_result(x) -> DynamicResult<String>`
- `dynamic_binary_result(x) -> DynamicResult<String>`
- `dynamic_int_result(x) -> DynamicResult<Int>`
- `dynamic_float_result(x) -> DynamicResult<Float>`
- `dynamic_bool_result(x) -> DynamicResult<Bool>`
- `dynamic_atom_result(x) -> DynamicResult<Atom>`
- `dynamic_list_result(x) -> DynamicResult<List<Dynamic>>`
- `dynamic_map_result(x) -> DynamicResult<Map[Any, Dynamic>>`

Structured error types:
- `DynamicResult<T> = Ok(T) | Err(DecodeError)`
- `DecodePathItem = Field(String) | Index(Int)`
- `DecodeError =`
  - `Expected(String)`
  - `MissingField(String)`
  - `MissingKey`
  - `IndexOutOfBounds(Int)`
  - `At(DecodePathItem, DecodeError)`
  - `OneOf(DecodeError, DecodeError)`
  - `InvalidJson`
  - `Message(String)`

Combinators:
- `decode_map(result, f)`
- `decode_then(result, f)`
- `decode_field(dynamic, key: String, decoder)`
- `decode_optional_field(dynamic, key: String, decoder)`
- `decode_field_or(dynamic, key: String, fallback, decoder)`
- `decode_index(dynamic, index, decoder)`
- `decode_list(dynamic, decoder)`
- `decode_optional(dynamic, decoder)`
- `decode_dict(dynamic, decoder)`
- `decode_one_of(dynamic, decoder_a, decoder_b)`

## Runtime Behavior

The extractors are checked at runtime.

If the shape does not match, Lux raises:

```text
{bad_dynamic, ExpectedKind, Value}
```

This is now the preferred JSON-decoding path. The older throwing extractors still
exist, but the `*_result` variants make failures explicit.

Nested decoder failures now carry a path:

```lux
match decode_field(payload, "name", |value| dynamic_string_result(value)) {
    DynamicResult::Err(
        DecodeError::At(
            DecodePathItem::Field(field),
            DecodeError::Expected(kind)
        )
    ) => field == "name" && kind == "string",
    _ => false
}
```

## Why This Exists

This gives Lux a real user-facing dynamic layer for:
- JSON
- message payloads
- boundary data from external systems
- gradual inspection before full typing

That is closer to Gleam's `dynamic` model than raw `Any`.

## What Is Still Missing

1. richer structured combinators like record/object decoders and schema-oriented composition
2. flow-sensitive refinement after `dynamic_is_*`
3. a bridge between `Dynamic` decoders and a future static `Json` ADT
4. decode/encode option surfaces where OTP JSON behavior matters
