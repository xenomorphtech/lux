# Namespace-Native Development

Lux source authority lives in SQLite, not in a project directory. The authoring
unit is one symbol in one namespace; callers do not load or resubmit a source file
or an entire namespace to change a function or type.

## Model

- Symbol identity is `namespace + kind + name`. A namespace has at most one
  function definition and one type definition for a given name.
- Parameter count belongs to the current definition and revision. It is derived
  from source, is never supplied as a selector, and is not announced by symbol,
  package, build, or CLI metadata. Function names are not overloaded by parameter
  count.
- Every symbol replacement creates an immutable, content-addressed symbol revision.
- `expected_generation` and `expected_revision_id` provide compare-and-swap
  concurrency. Stale edits return HTTP 409.
- A successful edit compiles, publishes one cumulative namespace generation, and
  creates an immutable execution snapshot in one API operation.
- A function edit replaces that source revision and atomically rebuilds only the
  transitive callers whose immutable artifacts reference its previous artifact.
  Those callers retain their source revision; unrelated bindings carry forward.
- A type edit is still one logical symbol edit; Lux internally rebuilds the
  namespace functions and commits their resulting bindings in the same generation.
- Artifact files and run directories are rebuildable materializations. SQLite is
  the code and provenance source of record.

Changesets remain as an immutable bulk-import/provenance format for older data and
multi-symbol ingestion. Byte-range changeset editing is a compatibility endpoint,
not the namespace-native development interface.

## Atomic symbol API

Create a function in a new namespace:

```bash
curl -sS -X POST http://127.0.0.1:4002/symbols/put \
  -H 'content-type: application/json' \
  --data '{
    "namespace":"math",
    "kind":"function",
    "symbol":"square",
    "source":"fn square(x: Int) -> Int { x * x }",
    "expected_generation":0
  }'
```

The response includes the new `generation`, `snapshot_id`, and
`symbol.revision_id`. Replace only that function using both optimistic guards:

```bash
curl -sS -X POST http://127.0.0.1:4002/symbols/put \
  -H 'content-type: application/json' \
  --data '{
    "namespace":"math",
    "kind":"function",
    "symbol":"square",
    "source":"fn square(x: Int) -> Int { pow(x, 2) }",
    "expected_generation":1,
    "expected_revision_id":"REVISION_ID"
  }'
```

The source must contain exactly the selected editable definition. Generic `extern`
or `use` declarations may accompany a newly imported function; Lux stores them as
hidden compile context, returns only the selected symbol from `get_symbol`, and
inherits that context on later replacements. An edit cannot silently add, remove,
or rename another symbol. Changing the selected function's parameters creates a
new revision of that same symbol and replaces its old snapshot binding by name. If
the change is compatible, Lux derives fresh artifacts for its transitive callers
and publishes the affected binding closure in the same generation. If a caller no
longer type-checks against the new signature, the edit is rejected before the
namespace generation advances.

Types use the same operation:

```json
{
  "namespace": "math",
  "kind": "type",
  "symbol": "Count",
  "source": "type Count = Int",
  "expected_generation": 2
}
```

`sandbox` and `capabilities` are optional. A replacement inherits them from the
current symbol unless explicitly supplied; a new symbol defaults to sandboxed with
no grants.

## Inspection

List symbol metadata at the current or a historical generation:

```bash
curl -sS 'http://127.0.0.1:4002/symbols?namespace=math'
curl -sS 'http://127.0.0.1:4002/symbols?namespace=math&generation=1'
```

Read exactly one symbol revision, including its source:

```bash
curl -sS 'http://127.0.0.1:4002/symbol?namespace=math&kind=function&symbol=square'
```

Namespace bindings, snapshots, and diffs remain available:

```bash
curl -sS http://127.0.0.1:4002/namespaces/math
curl -sS http://127.0.0.1:4002/snapshots/1
curl -sS 'http://127.0.0.1:4002/namespaces/math/diff?from=1&to=2'
```

Run from the snapshot returned by `symbols/put`:

```bash
curl -sS -X POST http://127.0.0.1:4002/run \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"target":"square","args":["7"]}'
```

## REPL

Start the service with a durable state directory, then the REPL:

```bash
LUX_HOME=/path/to/durable/lux-state cargo run -- --serve
cargo run --bin repl -- --namespace math
```

A symbol-native session is:

```text
:symbols
:symbol function square
:load
:show
:paste
fn square(x: Int) -> Int { x * x }
:end
:save
:run square 7
```

For a new symbol, skip `:load`. `:save` performs the atomic
compile/publish/snapshot operation and selects the returned snapshot. Legacy
changeset commands are available only under the `:legacy-*` prefix.

## MCP

Run the stdio MCP bridge against the HTTP service:

```bash
cargo run --bin lux-mcp -- --server http://127.0.0.1:4002
```

The authoring tools are:

- `lux.list_symbols`
- `lux.get_symbol`
- `lux.put_symbol`

`lux.put_symbol` has the same atomic and optimistic-concurrency semantics as the
HTTP endpoint. Changeset mutation tools are intentionally not exposed over MCP, so
an agent is not asked to calculate byte offsets or rewrite aggregate source.

## Durable state and protected resources

Keep `LUX_HOME` outside `target/`; the default `target/lux` is disposable. The
database and `resource-key.hex` must be backed up together. Runtime artifact, run,
and instance directories can be rebuilt.

Protected resources are encrypted and versioned in SQLite. The resource metadata
APIs and MCP tools never reveal values. A plaintext local-administration endpoint
requires `LUX_ALLOW_RESOURCE_REVEAL=1` and is deliberately absent from MCP.
