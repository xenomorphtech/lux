use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use lux::codegen::cranelift::NativeCompiler;
use lux::codegen::translate::Translator;
use lux::codegen::yggdrasil::YggdrasilCompiler;
use lux::driver::session::{CompileError, SecurityError, Session, SessionConfig};
use lux::service::api;
use lux::service::store::SqliteStore;
use lux::service::{CompiledPackage, LiveCodeService, ServiceError};
use lux::syntax::lexer::Lexer;
use lux::syntax::parser::{Parser, ParserOptions};

const DEFAULT_SERVICE_PORT: u16 = 4002;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--capability-host") {
        let Some(capability) = args.get(2) else {
            eprintln!("Missing capability name");
            process::exit(2);
        };
        if let Err(error) = lux::service::capability::run_host(capability) {
            eprintln!("Capability host error: {error}");
            process::exit(1);
        }
        return;
    }
    let workspace_dir = workspace_dir_from_env(env::var_os("LUX_HOME"));
    let artifacts_dir = workspace_dir.join("artifacts");
    let runs_dir = workspace_dir.join("runs");
    let db_path = workspace_dir.join("livecode.sqlite3");

    if args.len() < 2 {
        eprintln!("Usage: lux [options] <file.lux>");
        eprintln!("Options:");
        eprintln!("  --emit-core    Output Core Erlang only (don't compile to BEAM)");
        eprintln!("  --parse-only   Parse only (don't generate code)");
        eprintln!("  --publish NS   Compile and publish functions into namespace NS");
        eprintln!("  --snapshot NS  Create snapshot for namespace NS");
        eprintln!("  --run SNAP ID  Run symbol name or artifact hash in snapshot");
        eprintln!(
            "  --serve [ADDR] Start the local HTTP service (default 127.0.0.1:{})",
            DEFAULT_SERVICE_PORT
        );
        eprintln!("  --port PORT    Port for --serve when no explicit address is given");
        eprintln!("  --native       Compile to native executable via Cranelift");
        eprintln!("  --yggdrasil    Compile to verified Yggdrasil .yggm modules");
        eprintln!("  --sandbox      Compile with sandbox restrictions");
        process::exit(1);
    }

    let mut emit_core_only = false;
    let mut parse_only = false;
    let mut native_mode = false;
    let mut yggdrasil_mode = false;
    let mut sandbox = false;
    let mut publish_namespace = None;
    let mut serve_requested = false;
    let mut explicit_serve_addr = None;
    let mut serve_port = DEFAULT_SERVICE_PORT;
    let mut snapshot_namespace = None;
    let mut run_snapshot = None;
    let mut run_target = None;
    let mut positionals = Vec::new();

    let mut iter = args[1..].iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--emit-core" => emit_core_only = true,
            "--parse-only" => parse_only = true,
            "--native" => native_mode = true,
            "--yggdrasil" => yggdrasil_mode = true,
            "--sandbox" => sandbox = true,
            "--publish" => publish_namespace = iter.next().cloned(),
            "--port" => {
                let Some(port) = iter.next() else {
                    eprintln!("Missing value for --port");
                    process::exit(1);
                };
                serve_port = parse_port(port);
            }
            "--serve" => {
                serve_requested = true;
                if let Some(next) = iter.peek() {
                    if !next.starts_with('-') {
                        explicit_serve_addr = Some(iter.next().cloned().unwrap());
                    }
                }
            }
            "--snapshot" => snapshot_namespace = iter.next().cloned(),
            "--run" => {
                run_snapshot = iter.next().cloned();
                run_target = iter.next().cloned();
            }
            _ if !arg.starts_with('-') => positionals.push(arg.clone()),
            _ => {
                eprintln!("Unknown option: {}", arg);
                process::exit(1);
            }
        }
    }

    if let Some(addr) = resolve_serve_addr(serve_requested, explicit_serve_addr, serve_port) {
        let store = open_store(&workspace_dir, &db_path);
        let service = LiveCodeService::new(store);
        if let Err(err) = api::serve(&addr, service, workspace_dir.clone()) {
            eprintln!("Service error: {}", err);
            process::exit(1);
        }
        return;
    }

    if let Some(namespace) = snapshot_namespace {
        let store = open_store(&workspace_dir, &db_path);
        let mut service = LiveCodeService::new(store);
        let generation = positionals
            .first()
            .and_then(|value| value.parse::<i64>().ok());
        match service.create_snapshot(&namespace, generation) {
            Ok(snapshot_id) => {
                println!(
                    "Created snapshot {} for namespace {}",
                    snapshot_id, namespace
                );
                return;
            }
            Err(err) => {
                print_service_error(err);
                process::exit(1);
            }
        }
    }

    if let (Some(snapshot_id), Some(target)) = (run_snapshot, run_target) {
        let snapshot_id = snapshot_id.parse::<i64>().unwrap_or_else(|_| {
            eprintln!("Invalid snapshot id: {}", snapshot_id);
            process::exit(1);
        });
        let store = open_store(&workspace_dir, &db_path);
        let service = LiveCodeService::new(store);
        let execution_target = match service.resolve_execution_target(snapshot_id, &target) {
            Ok(Some(target)) => target,
            Ok(None) => {
                eprintln!("No binding or artifact found for {}", target);
                process::exit(1);
            }
            Err(err) => {
                print_service_error(err);
                process::exit(1);
            }
        };
        let run_dir = runs_dir.join(format!(
            "snapshot-{}-{}",
            snapshot_id, execution_target.artifact_hash
        ));
        let arg_terms = positionals;
        match service.execute_target(&execution_target, &arg_terms, &run_dir) {
            Ok(result) => {
                println!("{}", result.stdout);
                if let Some(beam_time_us) = result.beam_time_us {
                    println!("beam_time_us={}", beam_time_us);
                }
                return;
            }
            Err(err) => {
                print_service_error(err);
                process::exit(1);
            }
        }
    }

    let filename = positionals.first().cloned().unwrap_or_else(|| {
        eprintln!("No input file specified");
        process::exit(1);
    });

    let source = match fs::read_to_string(&filename) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", filename, e);
            process::exit(1);
        }
    };

    if native_mode && yggdrasil_mode {
        eprintln!("--native and --yggdrasil select different backends and cannot be combined");
        process::exit(1);
    }

    if parse_only {
        let tokens = Lexer::new(&source).tokenize();
        for token in &tokens {
            if let lux::syntax::token::TokenKind::Error(msg) = &token.kind {
                eprintln!("Lexer error at {:?}: {}", token.span, msg);
                process::exit(1);
            }
        }

        let mut parser = Parser::new_with_options(
            tokens,
            ParserOptions {
                allow_extern: !sandbox,
            },
        );
        let module = match parser.parse_module() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Parse error at {:?}: {}", e.span, e.message);
                process::exit(1);
            }
        };

        let module_name = module.name.clone().unwrap_or_else(|| {
            Path::new(&filename)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("main")
                .to_string()
        });

        println!("Parsed module: {}", module_name);
        println!("Items: {}", module.items.len());
        for item in &module.items {
            match item {
                lux::syntax::ast::Item::Function(f) => {
                    println!("  fn {}/{}", f.name, f.params.len());
                }
                lux::syntax::ast::Item::Enum(e) => {
                    println!("  enum {} ({} variants)", e.name, e.variants.len());
                }
                lux::syntax::ast::Item::Struct(s) => {
                    println!("  struct {} ({} fields)", s.name, s.fields.len());
                }
                lux::syntax::ast::Item::TypeAlias(t) => {
                    println!("  type {}", t.name);
                }
                lux::syntax::ast::Item::Extern(e) => {
                    println!("  extern \"{}\" ({} decls)", e.abi, e.decls.len());
                }
                lux::syntax::ast::Item::Use(u) => match &u.items {
                    Some(items) => println!("  use {}::{{{}}}", u.module, items.join(", ")),
                    None => println!("  use {}", u.module),
                },
            }
        }
        return;
    }

    // Native compilation via Cranelift backend
    if native_mode {
        compile_native(&source, &filename, sandbox, &artifacts_dir);
        return;
    }

    if yggdrasil_mode {
        compile_yggdrasil(&source, &filename, sandbox, &artifacts_dir);
        return;
    }

    let config = if sandbox {
        SessionConfig::sandboxed_default()
    } else {
        SessionConfig::trusted()
    };
    let store = open_store(&workspace_dir, &db_path);
    let mut service = LiveCodeService::new(store);

    let package = match service.compile_source(&source, config, &std::collections::HashMap::new()) {
        Ok(package) => package,
        Err(err) => {
            print_service_error(err);
            process::exit(1);
        }
    };
    if let Err(err) = service.store_package(&package) {
        print_service_error(err);
        process::exit(1);
    }
    if let Some(namespace) = publish_namespace {
        match service.publish_package(&namespace, &package) {
            Ok(generation) => println!(
                "Published namespace {} generation {}",
                namespace, generation
            ),
            Err(err) => {
                print_service_error(err);
                process::exit(1);
            }
        }
    }

    let module_name = if package.source_module == "main" {
        Path::new(&filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("main")
            .to_string()
    } else {
        package.source_module.clone()
    };

    let mut core_outputs: Vec<(PathBuf, String)> = Vec::new();
    for artifact in &package.artifacts {
        let core_path = artifacts_dir.join(format!("{}.core", artifact.artifact_hash));
        if let Err(e) = fs::write(&core_path, &artifact.core_source) {
            eprintln!("Error writing {}: {}", core_path.display(), e);
            process::exit(1);
        }
        println!("Generated: {}", core_path.display());
        core_outputs.push((core_path, artifact.core_source.clone()));
    }
    if core_outputs.is_empty() {
        eprintln!("No functions to compile");
        process::exit(1);
    }

    let metadata_path = artifacts_dir.join(format!("{}.meta.json", module_name));
    let metadata_json = build_metadata_json(&package);
    if let Err(e) = fs::write(&metadata_path, metadata_json) {
        eprintln!("Error writing {}: {}", metadata_path.display(), e);
        process::exit(1);
    }
    println!("Generated: {}", metadata_path.display());

    if emit_core_only {
        for (core_path, core_source) in &core_outputs {
            println!("\n// {}\n{}", core_path.display(), core_source);
        }
        return;
    }

    println!("Compiling with erlc...");
    let mut command = Command::new("erlc");
    command.arg("+from_core");
    command.current_dir(&artifacts_dir);
    for (core_path, _) in &core_outputs {
        let core_name = core_path.file_name().unwrap_or_else(|| {
            eprintln!("Invalid core output path: {}", core_path.display());
            process::exit(1);
        });
        command.arg(core_name);
    }
    let status = command.status();

    match status {
        Ok(s) if s.success() => {
            for (core_path, _) in &core_outputs {
                let beam_path = core_path.with_extension("beam");
                println!("Generated: {}", beam_path.display());
            }
        }
        Ok(s) => {
            eprintln!("erlc failed with exit code: {:?}", s.code());
            process::exit(1);
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("erlc not found. Install Erlang/OTP to compile to BEAM.");
                eprintln!("Core Erlang outputs:");
                for (core_path, _) in &core_outputs {
                    eprintln!("  {}", core_path.display());
                }
            } else {
                eprintln!("Error running erlc: {}", e);
            }
            process::exit(1);
        }
    }
}

fn compile_yggdrasil(source: &str, filename: &str, sandbox: bool, artifacts_dir: &Path) {
    let config = if sandbox {
        SessionConfig::sandboxed_default()
    } else {
        SessionConfig::trusted()
    };
    let mut session = Session::with_config(PathBuf::new(), config);
    let module = match session.compile_source(source) {
        Ok(module) => module,
        Err(error) => {
            print_compile_error(error);
            process::exit(1);
        }
    };

    let mut translator = Translator::new();
    let translated = translator.translate_function_modules(&module);
    if translated.modules.is_empty() {
        eprintln!("No functions to compile");
        process::exit(1);
    }

    let output = match YggdrasilCompiler::compile(
        &translated.modules,
        translated.entry_module.as_deref(),
        translated.entry_arity.unwrap_or(0),
    ) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("Yggdrasil compilation error: {error}");
            process::exit(1);
        }
    };

    if let Err(error) = fs::create_dir_all(artifacts_dir) {
        eprintln!("Error creating {}: {error}", artifacts_dir.display());
        process::exit(1);
    }

    let mut manifest_modules = Vec::with_capacity(output.modules.len());
    for compiled in &output.modules {
        let file = format!("{}.yggm", compiled.name);
        let path = artifacts_dir.join(&file);
        if let Err(error) = fs::write(&path, compiled.encode()) {
            eprintln!("Error writing {}: {error}", path.display());
            process::exit(1);
        }
        println!("Generated Yggdrasil module: {}", path.display());
        manifest_modules.push(serde_json::json!({
            "name": compiled.name,
            "file": file,
        }));
    }

    let entry = output.entry_module.as_ref().map(|module| {
        serde_json::json!({
            "module": module,
            "function": "apply",
            "arity": output.entry_arity,
        })
    });
    let aliases: Vec<serde_json::Value> = translated
        .metadata
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.source_name,
                "module": f.module_name,
                "arity": f.arity,
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "format": "YGGM1",
        "entry": entry,
        "modules": manifest_modules,
        "aliases": aliases,
    });
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("main");
    let manifest_path = artifacts_dir.join(format!("{stem}.ygg.json"));
    let manifest_json = serde_json::to_vec_pretty(&manifest).expect("JSON values are serializable");
    if let Err(error) = fs::write(&manifest_path, manifest_json) {
        eprintln!("Error writing {}: {error}", manifest_path.display());
        process::exit(1);
    }
    println!("Generated Yggdrasil manifest: {}", manifest_path.display());
    if let Some(entry) = &output.entry_module {
        println!("Yggdrasil entry: {entry}:apply/{}", output.entry_arity);
    }
}

fn compile_native(source: &str, filename: &str, sandbox: bool, artifacts_dir: &Path) {
    let config = if sandbox {
        SessionConfig::sandboxed_default()
    } else {
        SessionConfig::trusted()
    };

    // Parse and type-check
    let mut session = Session::with_config(PathBuf::new(), config);
    let module = match session.compile_source(source) {
        Ok(m) => m,
        Err(err) => {
            print_compile_error(err);
            process::exit(1);
        }
    };

    // Translate to Core Erlang IR (reuse existing translator)
    let mut translator = Translator::new();
    let translated = translator.translate_function_modules(&module);

    if translated.modules.is_empty() {
        eprintln!("No functions to compile");
        process::exit(1);
    }

    println!(
        "Compiling {} function(s) to native code via Cranelift...",
        translated.modules.len()
    );

    let entry_module = translated.entry_module.as_deref();
    let entry_arity = translated.entry_arity.unwrap_or(0);

    // Compile to native object code
    let output = match NativeCompiler::compile(&translated.modules, entry_module, entry_arity) {
        Ok(out) => out,
        Err(err) => {
            eprintln!("Native compilation error: {}", err);
            process::exit(1);
        }
    };

    // Determine output path
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a");
    let out_path = artifacts_dir.join(stem);

    if let Err(err) = fs::create_dir_all(artifacts_dir) {
        eprintln!("Error creating {}: {}", artifacts_dir.display(), err);
        process::exit(1);
    }

    // Link into native executable
    match NativeCompiler::link(&output, &out_path) {
        Ok(()) => {
            println!("Generated native executable: {}", out_path.display());
        }
        Err(err) => {
            eprintln!("Linking error: {}", err);
            eprintln!("Make sure a C compiler (cc) is available on your PATH.");
            process::exit(1);
        }
    }
}

fn default_serve_addr(port: u16) -> String {
    format!("127.0.0.1:{}", port)
}

fn resolve_serve_addr(
    serve_requested: bool,
    explicit_serve_addr: Option<String>,
    serve_port: u16,
) -> Option<String> {
    if !serve_requested {
        None
    } else {
        Some(explicit_serve_addr.unwrap_or_else(|| default_serve_addr(serve_port)))
    }
}

fn parse_port(value: &str) -> u16 {
    value.parse::<u16>().unwrap_or_else(|_| {
        eprintln!("Invalid port: {}", value);
        process::exit(1);
    })
}

fn workspace_dir_from_env(value: Option<std::ffi::OsString>) -> PathBuf {
    value
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target").join("lux"))
}

fn open_store(workspace_dir: &Path, db_path: &Path) -> SqliteStore {
    if let Err(err) = fs::create_dir_all(workspace_dir) {
        eprintln!("Error creating {}: {}", workspace_dir.display(), err);
        process::exit(1);
    }
    let artifacts_dir = workspace_dir.join("artifacts");
    if let Err(err) = fs::create_dir_all(&artifacts_dir) {
        eprintln!("Error creating {}: {}", artifacts_dir.display(), err);
        process::exit(1);
    }
    if let Err(err) = fs::create_dir_all(workspace_dir.join("runs")) {
        eprintln!(
            "Error creating {}: {}",
            workspace_dir.join("runs").display(),
            err
        );
        process::exit(1);
    }

    match SqliteStore::open(db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("Error opening {}: {}", db_path.display(), err);
            process::exit(1);
        }
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn build_metadata_json(package: &CompiledPackage) -> String {
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!(
        "  \"source_module\": \"{}\",\n",
        json_escape(&package.source_module)
    ));
    match &package.entry_module {
        Some(module) => {
            json.push_str("  \"entry\": {\n");
            json.push_str(&format!("    \"module\": \"{}\",\n", json_escape(module)));
            json.push_str("    \"function\": \"apply\"\n");
            json.push_str("  },\n");
        }
        _ => {
            json.push_str("  \"entry\": null,\n");
        }
    }
    json.push_str("  \"functions\": [\n");
    for (i, item) in package.artifacts.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!(
            "      \"source_name\": \"{}\",\n",
            json_escape(&item.source_name)
        ));
        json.push_str(&format!(
            "      \"body_hash\": \"{}\",\n",
            json_escape(&item.body_hash)
        ));
        json.push_str(&format!(
            "      \"abi_hash\": \"{}\",\n",
            json_escape(&item.abi_hash)
        ));
        json.push_str(&format!(
            "      \"module\": \"{}\",\n",
            json_escape(&item.artifact_hash)
        ));
        json.push_str(&format!(
            "      \"build_key\": \"{}\",\n",
            json_escape(&item.build_key)
        ));
        json.push_str("      \"function\": \"apply\",\n");
        json.push_str("      \"dependencies\": [");
        for (dep_index, dep) in item.dependencies.iter().enumerate() {
            if dep_index > 0 {
                json.push_str(", ");
            }
            json.push('"');
            json.push_str(&json_escape(dep));
            json.push('"');
        }
        json.push_str("],\n");
        json.push_str(&format!(
            "      \"hash\": \"{}\"\n",
            json_escape(&item.artifact_hash)
        ));
        json.push_str("    }");
        if i + 1 < package.artifacts.len() {
            json.push(',');
        }
        json.push('\n');
    }
    json.push_str("  ]\n");
    json.push('}');
    json.push('\n');
    json
}

fn print_service_error(err: ServiceError) {
    match err {
        ServiceError::Compile(err) => print_compile_error(err),
        ServiceError::Store(err) => {
            eprintln!("Store error: {}", err);
        }
        ServiceError::Io(err) => {
            eprintln!("I/O error: {}", err);
        }
        ServiceError::NoFunctions => {
            eprintln!("No functions to compile");
        }
        ServiceError::MissingEntryPoint => {
            eprintln!("Compiled package has no main entry point");
        }
        ServiceError::Timeout { timeout_ms } => {
            eprintln!("Execution timed out after {} ms", timeout_ms);
        }
        ServiceError::OutputLimitExceeded { limit_bytes } => {
            eprintln!("Execution output exceeded {} bytes", limit_bytes);
        }
        ServiceError::Conflict(message)
        | ServiceError::NotFound(message)
        | ServiceError::InvalidRequest(message) => {
            eprintln!("{}", message);
        }
    }
}

fn print_compile_error(err: CompileError) {
    match err {
        CompileError::Parse(e) => {
            eprintln!("Parse error at {:?}: {}", e.span, e.message);
        }
        CompileError::Type(e) => {
            eprintln!("Type error: {}", e);
        }
        CompileError::Io(e) => {
            eprintln!("I/O error: {}", e);
        }
        CompileError::Security(SecurityError::ExternDisallowed(span)) => {
            eprintln!(
                "Security error at {:?}: external declarations are disabled in sandbox mode",
                span
            );
        }
        CompileError::Security(SecurityError::ExternModuleDisallowed { module, span }) => {
            eprintln!(
                "Security error at {:?}: external module '{}' is not granted",
                span, module
            );
        }
        CompileError::Security(SecurityError::ImportDisallowed { module, span }) => {
            eprintln!(
                "Security error at {:?}: import '{}' is not allowed in sandbox mode",
                span, module
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_metadata_json, resolve_serve_addr, workspace_dir_from_env};
    use lux::service::{CompiledPackage, FunctionArtifact};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn workspace_defaults_to_target_lux() {
        assert_eq!(workspace_dir_from_env(None), PathBuf::from("target/lux"));
    }

    #[test]
    fn workspace_honors_lux_home_override() {
        assert_eq!(
            workspace_dir_from_env(Some(OsString::from("/tmp/lux-home"))),
            PathBuf::from("/tmp/lux-home")
        );
    }

    #[test]
    fn serve_addr_uses_port_when_no_explicit_address_is_given() {
        assert_eq!(
            resolve_serve_addr(true, None, 4010),
            Some("127.0.0.1:4010".to_string())
        );
    }

    #[test]
    fn serve_addr_prefers_explicit_address_over_port() {
        assert_eq!(
            resolve_serve_addr(true, Some("0.0.0.0:9999".to_string()), 4010),
            Some("0.0.0.0:9999".to_string())
        );
    }

    #[test]
    fn build_metadata_does_not_announce_symbol_arities() {
        let package = CompiledPackage {
            source_module: "example".to_string(),
            entry_module: Some("artifact_main".to_string()),
            entry_arity: Some(1),
            artifacts: vec![FunctionArtifact {
                source_name: "main".to_string(),
                body_hash: "body_main".to_string(),
                abi_hash: "abi_main".to_string(),
                artifact_hash: "artifact_main".to_string(),
                build_key: "build_main".to_string(),
                arity: 1,
                core_source: String::new(),
                dependencies: Vec::new(),
            }],
        };

        let metadata = build_metadata_json(&package);
        assert!(!metadata.contains("\"arity\""));
        assert!(metadata.contains("\"source_name\": \"main\""));
    }
}
