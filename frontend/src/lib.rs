//! The Lux compiler frontend: lexer, parser, type inference, Core lowering,
//! and the Erlang/Yggdrasil backends. `#![no_std]`-capable so it can run
//! inside the Yggdrasil kernel; the `std` feature (default) adds nothing but
//! keeps host builds on the standard prelude.
//!
//! Filesystem, sqlite, HTTP, and the Cranelift native backend all live in the
//! parent `lux` crate — this crate does no I/O at all. `use` resolution goes
//! through the `driver::uses::SourceProvider` trait.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod codegen;
pub mod collections;
pub mod driver;
pub mod syntax;
pub mod types;

pub(crate) mod prelude {
    #![allow(unused_imports)]
    pub use alloc::borrow::ToOwned;
    pub use alloc::boxed::Box;
    pub use alloc::format;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec;
    pub use alloc::vec::Vec;
}

use alloc::string::String;
use alloc::vec::Vec;

/// One in-memory compile: `use`-expanded source → verified content-addressed
/// Yggdrasil modules. This is `main.rs`'s `compile_yggdrasil` minus every
/// filesystem/manifest concern — the callable core for hosts and kernels.
pub struct FrontendOutput {
    /// (content hash = module name, encoded `.yggm` bytes) per function.
    pub modules: Vec<(String, Vec<u8>)>,
    /// (source-level function name, module hash, arity).
    pub aliases: Vec<(String, String, u8)>,
    /// `main/0`'s module hash, when the source defines one.
    pub entry: Option<(String, u8)>,
}

#[derive(Debug)]
pub enum FrontendError {
    /// `use` expansion failed (unresolvable library).
    Expand(String),
    Compile(driver::session::CompileError),
    /// Core → Yggdrasil backend failure.
    Backend(alloc::string::String),
}

pub fn compile_to_yggdrasil(
    source: &str,
    provider: &dyn driver::uses::SourceProvider,
    config: driver::session::SessionConfig,
) -> Result<FrontendOutput, FrontendError> {
    let expanded =
        driver::uses::expand_uses_with(source, provider).map_err(FrontendError::Expand)?;
    let mut session = driver::session::Session::with_config(config);
    let module = session
        .compile_source(&expanded)
        .map_err(FrontendError::Compile)?;

    let mut translator = codegen::translate::Translator::new();
    let translated = translator.translate_function_modules(&module);

    let output = codegen::yggdrasil::YggdrasilCompiler::compile(
        &translated.modules,
        translated.entry_module.as_deref(),
        translated.entry_arity.unwrap_or(0),
    )
    .map_err(|e| FrontendError::Backend(alloc::format!("{e}")))?;

    Ok(FrontendOutput {
        modules: output
            .modules
            .iter()
            .map(|m| (m.name.clone(), m.encode()))
            .collect(),
        aliases: translated
            .metadata
            .iter()
            .map(|f| (f.source_name.clone(), f.module_name.clone(), f.arity as u8))
            .collect(),
        entry: output
            .entry_module
            .clone()
            .map(|m| (m, output.entry_arity as u8)),
    })
}
