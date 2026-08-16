# Lux

Minimalist compiler targeting Erlang/BEAM with content-addressed function modules.

## License

MIT. See [LICENSE](./LICENSE).

## Run Example

This compiles one `.core` and one `.beam` per function (module names are hashes), and writes metadata for introspection.

```bash
cd /home/sdancer/lux
cargo run -- examples/fib.lux
```

Generated artifacts live under `target/lux/` by default:

```bash
cat target/lux/artifacts/fib.meta.json
```

Run the compiled entry function (replace module hash if different):

```bash
erl -noshell -pa /home/sdancer/lux/target/lux/artifacts -eval "io:format(\"~p~n\", ['e8a56de9f0f836e0':apply()]), halt()."
```

Expected output for `examples/fib.lux`:

```text
55
```

## IPv4/TCP Stack

Lux includes a pure packet-oriented IPv4/TCP implementation in
[`examples/tcp_ip.lux`](examples/tcp_ip.lux). It validates and emits IPv4 and TCP
checksums, implements active/passive handshakes, ordered data/ACK handling, reset,
and orderly close states. Its deterministic protocol vectors run with:

```bash
cargo test --test tcp_ip_stack
```

See [`docs/tcp-ip-stack.md`](docs/tcp-ip-stack.md) for the adapter contract,
supported behavior, and current Yggdrasil packet-buffer boundary.

## Yggdrasil Backend

Lux can compile its content-addressed function modules directly to the
Yggdrasil register-bytecode format. The sibling Yggdrasil checkout is consumed
through `../yggdrasil/crates/ygg-bytecode`, and every generated module is run
through Yggdrasil's verifier before it is written:

```bash
cargo run -- --yggdrasil examples/fib.lux
```

The command writes one `.yggm` file per hot-code module plus
`target/lux/artifacts/fib.ygg.json`. The manifest names every module and records
the Yggdrasil entry point, for example `HASH:apply/0`; load modules under those
exact names so `CALL_EXT` can resolve content-addressed Lux calls.

The backend currently covers integers, atoms, tuples, lists, local and external
calls, arithmetic, comparisons, literal/structural cases, basic receive/send,
and Yggdrasil debug printing. Constructs that the current Yggdrasil instruction
set cannot represent—such as floats, binaries/strings, maps, closures, receive
timeouts, and try/catch—produce an explicit compile-time backend error.

Set `LUX_HOME` to use a different workspace:

```bash
LUX_HOME=/tmp/lux-dev cargo run -- examples/fib.lux
```

## Local Service

Start the local HTTP service:

```bash
LUX_HOME=/path/to/durable/lux-state cargo run -- --serve
```

`LUX_HOME` contains the SQLite source-of-record plus rebuildable runtime
materializations. Set it outside `target/` for durable development; the default
`target/lux` location is intended for disposable local evaluation.

Health check:

```bash
curl -sS http://127.0.0.1:4002/health
```

The durable development unit is an atomic function or type symbol in a database-backed
namespace. Create a function without creating or resubmitting a `.lux` file:

```bash
curl -sS -X POST http://127.0.0.1:4002/symbols/put \
  -H 'content-type: application/json' \
  --data '{"namespace":"dev","kind":"function","symbol":"fib","source":"fn fib(n) { if n <= 1 { n } else { fib(n - 1) + fib(n - 2) } }","expected_generation":0}'
```

That single operation validates the symbol identity, compiles it against the current
namespace, publishes generation 1, and creates a snapshot. To replace the function,
send only its new definition plus the returned generation and revision ID:

```bash
curl -sS -X POST http://127.0.0.1:4002/symbols/put \
  -H 'content-type: application/json' \
  --data '{"namespace":"dev","kind":"function","symbol":"fib","source":"fn fib(n) { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }","expected_generation":1,"expected_revision_id":"REVISION_ID"}'
```

Because artifacts contain immutable dependency hashes, a replacement also rebuilds
the transitive callers of the old artifact in that same atomic generation. Their
source revisions do not change, and unrelated bindings are carried forward.

Inspect symbol metadata or one symbol’s immutable source revision:

```bash
curl -sS 'http://127.0.0.1:4002/symbols?namespace=dev'
curl -sS 'http://127.0.0.1:4002/symbol?namespace=dev&kind=function&symbol=fib'
```

Type symbols use `kind: "type"`; a type edit rebuilds affected published function
artifacts and commits all resulting bindings in the same generation. The older
changeset endpoints remain for bulk import and provenance compatibility, not normal
editing.

Use the namespace REPL for the same workflow interactively:

```bash
cargo run --bin repl -- --namespace dev
```

Or expose the same symbol-native operations over MCP stdio while the service is
running:

```bash
cargo run --bin lux-mcp -- --server http://127.0.0.1:4002
```

The authoring MCP surface is `lux.list_symbols`, `lux.get_symbol`, and
`lux.put_symbol`. `lux.put_symbol` is the atomic compile/publish/snapshot operation;
it never asks an agent to calculate source offsets or submit a whole namespace.

See [Namespace-Native Development](docs/namespaces-and-snapshots.md) for atomic
symbol revisions, optimistic concurrency, MCP tools, and inspection endpoints.
The current Albion guest-account port and its remaining capability work are
tracked in [Albion Clientless Port](docs/albion-clientless-port.md).

Run published code from a snapshot:

```bash
curl -sS -X POST http://127.0.0.1:4002/run \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"target":"main"}'
```

Deploy a long-running instance from a snapshot:

```bash
curl -sS -X POST http://127.0.0.1:4002/deploy \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"target":"main"}'
```

List and inspect instances:

```bash
curl -sS http://127.0.0.1:4002/instances
```

```bash
curl -sS http://127.0.0.1:4002/instances/<instance_id>
```

Stop an instance:

```bash
curl -sS -X POST http://127.0.0.1:4002/instances/<instance_id>/stop
```

Evaluate an ephemeral snippet against a snapshot:

```bash
curl -sS -X POST http://127.0.0.1:4002/eval \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"source":"fn main() { fib(10) }"}'
```

Execution limits can be set per request:

```bash
curl -sS -X POST http://127.0.0.1:4002/eval \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"source":"fn main() { main() }","limits":{"timeout_ms":50}}'
```

```bash
curl -sS -X POST http://127.0.0.1:4002/eval \
  -H 'content-type: application/json' \
  --data '{"snapshot_id":1,"source":"fn main() { \"...large value...\" }","limits":{"output_limit_bytes":1024}}'
```

Inspect recent execution summaries:

```bash
curl -sS http://127.0.0.1:4002/executions
```

Inspect namespaces and frozen snapshots:

```bash
curl -sS http://127.0.0.1:4002/namespaces
```

```bash
curl -sS http://127.0.0.1:4002/namespaces/dev
```

```bash
curl -sS http://127.0.0.1:4002/snapshots/1
```

Diff two namespace generations:

```bash
curl -sS 'http://127.0.0.1:4002/namespaces/dev/diff?from=1&to=2'
```

Filter and page execution summaries:

```bash
curl -sS 'http://127.0.0.1:4002/executions?request_kind=eval&status=success&limit=10'
```

Prune old execution summaries:

```bash
curl -sS -X POST http://127.0.0.1:4002/executions/prune \
  -H 'content-type: application/json' \
  --data '{"finished_before_ms":9999999999999}'
```

## POC REPL

Start the service first:

```bash
cargo run -- --serve
```

Use a different port:

```bash
cargo run -- --serve --port 4010
```

Then start the REPL client:

```bash
cargo run --bin repl
```

Point the REPL at a specific port:

```bash
cargo run --bin repl -- --port 4010
```

Minimal session:

```text
:namespace repl
:symbol function fib 1
:paste
fn fib(n) { if n <= 1 { n } else { fib(n - 1) + fib(n - 2) } }
:end
:save
fib(10)
```

The REPL is a thin client over the HTTP service. Bare lines are evaluated as expressions by wrapping them in `fn main() { ... }`.

## Protected Namespace Resources

Lux stores credentials and device/session state as encrypted, versioned namespace
resources in SQLite. By default the service creates or loads
`$LUX_HOME/resource-key.hex`; on Unix it requires a private regular file and creates
it with mode `0600`. `LUX_MASTER_KEY_FILE` selects another key file and
`LUX_MASTER_KEY_HEX` provides an explicit process-level override.

Back up the key with the database: losing either one loses access to the protected
values. Normal resource APIs and MCP tools expose metadata only. Plaintext reveal is
a local administrative path gated by `LUX_ALLOW_RESOURCE_REVEAL=1`, and is never an
MCP tool.

## Security Model

The intended security boundary for Lux is the language and type/capability system, not OS sandboxing.

- No syscall / network isolation is intended as the primary enforcement mechanism.
- No per-job OS sandboxing is intended as the primary enforcement mechanism.
- The goal is to make unsafe APIs unavailable at compile time by not admitting them into the typed execution environment.
- In other words, code should be unable to name or typecheck against capabilities that are not explicitly present in the environment.

This is meant to be a stronger and more principled model than relying on ad hoc runtime blocking. It should ultimately stand on the correctness of the language, type system, and capability surface, rather than on post hoc process restrictions.

Current runtime limits such as timeouts and output caps are still useful operational controls, but they are not the long-term security story.
