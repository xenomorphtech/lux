# Lux

A minimalist self-hosted compiler with Rust-like syntax targeting Erlang/BEAM.

## Overview

| Aspect | Choice |
|--------|--------|
| **Name** | Lux |
| **Syntax** | Rust-like (`fn`, `let`, `match`, `enum`) |
| **Type System** | Hindley-Milner with full inference |
| **Target** | Core Erlang → BEAM bytecode |
| **FFI** | Raw Erlang processes (`spawn`, `send`, `receive`) |
| **Bootstrap** | Rust first → self-hosted |

## Syntax

```lux
mod counter

enum Message {
    Inc,
    Get(Pid),
}

fn counter(n: Int) -> Never {
    receive {
        Message::Inc => counter(n + 1),
        Message::Get(sender) => {
            send(sender, n)
            counter(n)
        }
    }
}

fn main() -> () {
    let pid = spawn(|| counter(0))
    send(pid, Message::Inc)
    send(pid, Message::Get(self()))
    receive { n => print(n) }
}
```

### Core Constructs

```lux
// Functions
fn add(a: Int, b: Int) -> Int {
    a + b
}

// Type inference
fn example() -> Int {
    let x = 42           // inferred as Int
    let y: Int = x + 1   // explicit annotation
    y
}

// Generics
fn identity<T>(x: T) -> T { x }

// Enums (ADTs)
enum Option<T> {
    Some(T),
    None,
}

// Pattern matching
fn unwrap<T>(opt: Option<T>, default: T) -> T {
    match opt {
        Option::Some(val) => val,
        Option::None => default,
    }
}

// Records
type Point = { x: Int, y: Int }

// Lambdas
let double = |x| x * 2

// Lists
let nums = [1, 2, 3]
let [head | tail] = nums

// Atoms
let status = :ok
```

### Process Primitives

```lux
// Spawn a process
let pid = spawn(|| server_loop(state))

// Send message (async)
send(pid, :hello)

// Receive with pattern matching
receive {
    :ping(sender) => send(sender, :pong),
    :quit => exit(:normal),
    msg => println("Unknown: {}", msg),
}

// Get own PID
let me = self()
```

### Erlang Interop

```lux
// External function declarations
extern "erlang" {
    fn lists:reverse<T>(List<T>) -> List<T>
    fn io:format(String, List<Any>) -> Atom
}

// Usage
fn example() -> () {
    let reversed = lists:reverse([1, 2, 3])
    io:format("Result: ~p~n", [reversed])
}
```

## Type System

### Primitives

| Lux | Erlang |
|-----|--------|
| `Int` | `integer()` |
| `Float` | `float()` |
| `Bool` | `true \| false` |
| `String` | `binary()` |
| `Atom` | `atom()` |
| `Pid` | `pid()` |
| `Ref` | `reference()` |

### Compound Types

| Lux | Erlang |
|-----|--------|
| `List<T>` | `[T]` |
| `Tuple2<A,B>` | `{A, B}` |
| `Map<K,V>` | `#{K => V}` |
| `fn(A) -> B` | `fun((A) -> B)` |

### Special Types

| Type | Purpose |
|------|---------|
| `Option<T>` | `Some(T) \| None` |
| `Result<T,E>` | `Ok(T) \| Err(E)` |
| `Never` | Non-returning (infinite loops) |
| `Any` | FFI escape hatch |

## Compiler Architecture

```
Source (.lux)
    │
    ▼
┌─────────┐
│  Lexer  │  → Token stream
└─────────┘
    │
    ▼
┌─────────┐
│ Parser  │  → Untyped AST
└─────────┘
    │
    ▼
┌─────────┐
│ Desugar │  → Core AST
└─────────┘
    │
    ▼
┌───────────┐
│ Type Infer│  → Typed AST
│   (HM)    │
└───────────┘
    │
    ▼
┌───────────┐
│ Erlang Gen│  → Core Erlang
└───────────┘
    │
    ▼
┌───────────┐
│   Emit    │  → .core files
└───────────┘
    │
    ▼
   erlc → .beam
```

## Project Structure

```
lux/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI entry point
│   ├── lib.rs
│   │
│   ├── syntax/
│   │   ├── mod.rs
│   │   ├── token.rs         # Token definitions
│   │   ├── lexer.rs         # Tokenizer
│   │   ├── ast.rs           # Surface AST
│   │   ├── parser.rs        # Recursive descent
│   │   └── span.rs          # Source locations
│   │
│   ├── types/
│   │   ├── mod.rs
│   │   ├── types.rs         # Type representation
│   │   ├── infer.rs         # HM inference
│   │   ├── unify.rs         # Unification
│   │   ├── subst.rs         # Substitution
│   │   └── env.rs           # Type environment
│   │
│   ├── codegen/
│   │   ├── mod.rs
│   │   ├── erlang.rs        # Core Erlang AST
│   │   ├── emit.rs          # .core emission
│   │   └── builtins.rs      # Runtime primitives
│   │
│   └── driver/
│       ├── mod.rs
│       └── session.rs       # Compilation session
│
├── runtime/
│   └── lux_runtime.erl      # Minimal Erlang runtime
│
├── stdlib/
│   ├── prelude.lux
│   ├── option.lux
│   ├── result.lux
│   ├── list.lux
│   └── process.lux
│
└── tests/
    ├── lexer/
    ├── parser/
    ├── typecheck/
    └── integration/
```

## Implementation Phases

### Phase 1: Lexer & Parser ✓
- [x] Token definitions (literals, keywords, operators)
- [x] Lexer with span tracking
- [x] Surface AST types
- [x] Recursive descent parser
- [x] Error reporting with source locations

### Phase 2: Type System ✓
- [x] Type representation
- [x] Type environment and schemes
- [x] Fresh variable generation
- [x] Unification algorithm
- [x] Occurs check
- [x] HM inference for expressions
- [x] Let-polymorphism (generalization)
- [x] Type error messages

### Phase 3: Code Generation ✓
- [x] Core Erlang AST types
- [x] Typed AST → Core Erlang translation
- [x] Pattern match compilation (basic)
- [x] Core Erlang text emitter
- [x] `erlc` integration

### Phase 4: Erlang FFI ✓
- [x] `spawn(fn)` primitive
- [x] `send(pid, msg)` primitive
- [x] `receive { ... }` expression (with primops)
- [x] `self()` primitive
- [x] `print()` builtin
- [x] External function declarations (parsing)

### Phase 5: Standard Library
- [ ] Option<T>, Result<T,E>
- [ ] List operations
- [ ] String manipulation
- [ ] File I/O
- [ ] Process utilities

### Phase 6: Self-Hosting
- [ ] Implement compiler in Lux
- [ ] Bootstrap verification (Rust vs Lux output match)
- [ ] Archive Rust implementation

## Example Compilation

### Input: factorial.lux

```lux
mod factorial

fn factorial(n: Int) -> Int {
    if n <= 1 {
        1
    } else {
        n * factorial(n - 1)
    }
}

fn main() -> () {
    let result = factorial(5)
    print(result)
}
```

### Output: factorial.core

```erlang
module 'factorial'
    ['factorial'/1, 'main'/0]
attributes []

'factorial'/1 = fun (N) ->
    case call 'erlang':'=<'(N, 1) of
        'true' -> 1
        'false' ->
            call 'erlang':'*'(
                N,
                apply 'factorial'/1 (call 'erlang':'-'(N, 1))
            )
    end

'main'/0 = fun () ->
    let Result = apply 'factorial'/1 (5)
    in call 'io':'format'("~p~n", [Result])

end
```

## Key Design Decisions

1. **Core Erlang target** - More stable than raw BEAM bytecode, leverages `erlc` optimizations
2. **Full HM inference** - Types are inferred; explicit annotations optional but allowed
3. **`:atom` syntax** - Distinguishes atoms from variables (like Elixir)
4. **`receive` as expression** - Returns the matched value, not just side effects
5. **`Never` type** - For processes that loop forever
6. **Minimal runtime** - Thin wrapper over Erlang/OTP, not a framework

## Self-Hosting Requirements

The compiler must be able to compile itself. Required features:

| Feature | Usage |
|---------|-------|
| File I/O | Read source files, write .core output |
| Strings | Lexer, error messages, emitter |
| Pattern matching | AST traversal everywhere |
| ADTs (enum) | AST nodes, types, IR |
| Recursion | Parsing, inference, codegen |
| Process spawn | Shell out to `erlc` |

## Getting Started

```bash
cd lux
cargo build
cargo run -- examples/hello.lux
cargo test
```

## Current Status

The Lux compiler can compile programs with processes to BEAM bytecode:

```bash
$ ./target/release/lux examples/counter.lux
Generated: counter.core
Compiling with erlc...
Generated: counter.beam

$ erl -noshell -eval 'counter:main().' -s init stop
```

**Working:**
- Complete lexer and parser for full syntax
- Hindley-Milner type inference engine
- Core Erlang translation and emission
- Integration with `erlc` for BEAM compilation
- Recursive functions, if/else, pattern matching
- Arithmetic and comparison operators
- Process primitives (spawn, send, receive)
- Enum/ADT codegen (tagged tuples)
- print() builtin

**Examples:**
- `hello.lux` - Factorial, returns 120
- `fib.lux` - Fibonacci, returns 55
- `counter.lux` - Process with receive loop
- `option.lux` - Enum/ADT with Option type
- `print_test.lux` - Print builtin demo

**In Progress:**
- Standard library (Option, Result, List)
- Timeout in receive
- Guards in pattern matching
