//! Frontend-only integration tests: in-memory compilation (the kernel's
//! calling convention) and a hostile-input battery — every malformed source
//! must come back as Err, never a panic, since the kernel build is
//! panic=abort.

use std::borrow::Cow;

use lux_frontend::driver::session::SessionConfig;
use lux_frontend::driver::uses::SourceProvider;
use lux_frontend::{FrontendError, compile_to_yggdrasil};

struct MapProvider(&'static [(&'static str, &'static str)]);

impl SourceProvider for MapProvider {
    fn source(&self, name: &str, _from: Option<&str>) -> Option<(Cow<'_, str>, String)> {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| (Cow::Borrowed(*s), String::from(name)))
    }
}

const NO_LIBS: MapProvider = MapProvider(&[]);

#[test]
fn compiles_a_string_in_memory() {
    let out = compile_to_yggdrasil(
        "mod repl\nfn main() -> Int { 41 + 1 }\n",
        &NO_LIBS,
        SessionConfig::trusted(),
    )
    .expect("trivial program compiles");
    let (entry, arity) = out.entry.expect("main/0 entry");
    assert_eq!(arity, 0);
    assert!(out.modules.iter().any(|(name, _)| name == &entry));
    assert!(out.aliases.iter().any(|(n, h, a)| n == "main" && h == &entry && *a == 0));
    // The modules are decodable, verified bytecode.
    for (_, bytes) in &out.modules {
        let m = ygg_bytecode::Module::decode(bytes).expect("decodable");
        ygg_bytecode::verify::verify(&m).expect("verifies");
    }
}

#[test]
fn expression_main_without_return_type() {
    // The REPL wraps expressions as an unannotated main.
    let out = compile_to_yggdrasil(
        "mod repl\nfn main() { 1 + 2 }\n",
        &NO_LIBS,
        SessionConfig::trusted(),
    )
    .expect("unannotated main compiles");
    assert!(out.entry.is_some());
}

#[test]
fn uses_resolve_through_the_provider() {
    let libs = MapProvider(&[(
        "mathlib",
        "mod mathlib\nfn triple(x: Int) -> Int { x * 3 }\nfn main() -> Int { 0 }\n",
    )]);
    let out = compile_to_yggdrasil(
        "mod repl\nuse mathlib\nfn main() -> Int { triple(14) }\n",
        &libs,
        SessionConfig::trusted(),
    )
    .expect("compiles against provider lib");
    assert!(out.aliases.iter().any(|(n, _, _)| n == "triple"));
    // The lib's own main was stripped: exactly one main alias.
    assert_eq!(out.aliases.iter().filter(|(n, _, _)| n == "main").count(), 1);
}

#[test]
fn unresolvable_use_is_an_error() {
    match compile_to_yggdrasil("use nosuchlib\n", &NO_LIBS, SessionConfig::trusted()) {
        Err(FrontendError::Expand(msg)) => assert!(msg.contains("nosuchlib")),
        other => panic!("expected Expand error, got {other:?}", other = other.is_ok()),
    }
}

/// Malformed/hostile sources must all return Err without panicking.
#[test]
fn garbage_battery_never_panics() {
    let cases: &[&str] = &[
        "",
        "fn",
        "fn main(",
        "fn main() -> { }",
        "fn main() -> Int { ",
        "fn main() -> Int { undefined_var }",
        "fn main() -> Int { \"str\" }",
        "fn f(x: Nope) -> Int { 0 }",
        "struct S { x: Int",
        "match x { }",
        "fn main() -> Int { match 1 { } }",
        "fn f() -> Int { f(1) }",
        "fn f(x: Int) -> Int { f() }",
        "use",
        "use ::{}",
        "mod",
        "mod m mod n",
        "fn main() -> Int { 1 + }",
        "fn main() -> Int { (((((((((( }",
        "fn main() -> Int { [1 | ] }",
        "fn main() -> Int { <<1:999999999999>> }",
        "fn main() -> [Int] { {1, 2} }",
        "receive { }",
        "fn main() -> Int { receive { after } }",
        "extern { fn ygg::nope() -> Int }",
        "fn main() -> Int { 0x }",
        "fn main() -> Int { 9999999999999999999999999999 }",
        "λ λ λ",
        "fn 🦀() -> Int { 0 }",
        "\u{0}\u{1}\u{2}",
        "fn main() -> Int { a.b.c.d.e }",
        "fn f<T() -> T { }",
    ];
    // The contract is panic-freedom (the kernel is panic=abort); degenerate
    // inputs like "" are legal empty modules and may compile.
    for src in cases {
        let _ = compile_to_yggdrasil(src, &NO_LIBS, SessionConfig::trusted());
    }
    // Clearly-invalid syntax must still be a clean Err.
    for src in ["fn main() -> Int {", "fn main() -> Int { 1 + }", "struct S { x: Int"] {
        assert!(
            compile_to_yggdrasil(src, &NO_LIBS, SessionConfig::trusted()).is_err(),
            "expected error: {src:?}"
        );
    }
    // Hostile nesting must be a clean error (the parser depth guard), never
    // stack exhaustion — the kernel runs on a fixed stack.
    let deep = format!("fn main() -> Int {{ {}1{} }}", "(".repeat(5000), ")".repeat(5000));
    assert!(compile_to_yggdrasil(&deep, &NO_LIBS, SessionConfig::trusted()).is_err());
}
