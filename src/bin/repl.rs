use std::env;
use std::io::{self, Write};

use serde_json::{Value, json};

struct ReplState {
    server_url: String,
    namespace: String,
    symbol_kind: String,
    symbol: String,
    symbol_revision_id: Option<String>,
    generation: Option<i64>,
    changeset: String,
    revision_id: Option<String>,
    capabilities: Vec<String>,
    snapshot_id: Option<i64>,
    sandbox: bool,
    timeout_ms: u64,
    output_limit_bytes: usize,
    draft: Option<String>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            server_url: default_server_url(4002),
            namespace: "repl".to_string(),
            symbol_kind: "function".to_string(),
            symbol: "main".to_string(),
            symbol_revision_id: None,
            generation: None,
            changeset: "scratch".to_string(),
            revision_id: None,
            capabilities: Vec::new(),
            snapshot_id: None,
            sandbox: true,
            timeout_ms: 5_000,
            output_limit_bytes: 65_536,
            draft: None,
        }
    }
}

fn main() {
    let mut state = ReplState::default();
    let mut explicit_server_url = None;
    let mut server_port = 4002;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let Some(port) = args.next() else {
                    eprintln!("Missing value for --port");
                    std::process::exit(1);
                };
                server_port = parse_port(&port);
            }
            "--server" => {
                let Some(url) = args.next() else {
                    eprintln!("Missing value for --server");
                    std::process::exit(1);
                };
                explicit_server_url = Some(url);
            }
            "--namespace" => {
                let Some(namespace) = args.next() else {
                    eprintln!("Missing value for --namespace");
                    std::process::exit(1);
                };
                state.namespace = namespace;
            }
            _ => {
                eprintln!("Unknown option: {}", arg);
                std::process::exit(1);
            }
        }
    }
    state.server_url = resolve_server_url(explicit_server_url, server_port);

    if let Err(err) = ensure_service_available(&state.server_url) {
        eprintln!("{}", err);
        std::process::exit(1);
    }

    print_help();

    let stdin = io::stdin();
    loop {
        print_prompt(&state);
        let mut chunk = String::new();
        if stdin.read_line(&mut chunk).is_err() {
            eprintln!("Failed to read input");
            break;
        }
        for line in chunk.lines() {
            match process_repl_line(&mut state, line) {
                Ok(ReplControl::Continue) => {}
                Ok(ReplControl::Exit) => return,
                Err(err) => eprintln!("{}", err),
            }
        }
    }
}

fn default_server_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

fn resolve_server_url(explicit_server_url: Option<String>, port: u16) -> String {
    explicit_server_url.unwrap_or_else(|| default_server_url(port))
}

fn parse_port(value: &str) -> u16 {
    value.parse::<u16>().unwrap_or_else(|_| {
        eprintln!("Invalid port: {}", value);
        std::process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ReplControl, ReplState, process_repl_line, resolve_server_url,
        strip_bracketed_paste_markers,
    };

    #[test]
    fn explicit_server_url_wins_over_port() {
        assert_eq!(
            resolve_server_url(Some("http://example.com:9999".to_string()), 4010),
            "http://example.com:9999"
        );
    }

    #[test]
    fn port_is_used_when_no_explicit_server_url_is_given() {
        assert_eq!(resolve_server_url(None, 4010), "http://127.0.0.1:4010");
    }

    #[test]
    fn blank_repl_line_is_ignored() {
        let mut state = ReplState::default();
        let control = process_repl_line(&mut state, "   ").unwrap();
        assert!(matches!(control, ReplControl::Continue));
    }

    #[test]
    fn bracketed_paste_markers_are_stripped() {
        assert_eq!(
            strip_bracketed_paste_markers("\u{1b}[200~:namespace warmtest\u{1b}[201~"),
            ":namespace warmtest"
        );
    }
}

enum ReplControl {
    Continue,
    Exit,
}

fn process_repl_line(state: &mut ReplState, line: &str) -> Result<ReplControl, String> {
    let normalized = strip_bracketed_paste_markers(line);
    let line = normalized.trim();
    if line.is_empty() {
        return Ok(ReplControl::Continue);
    }
    if line.starts_with(':') {
        return handle_command(state, line);
    }
    eval_expression(state, line)?;
    Ok(ReplControl::Continue)
}

fn strip_bracketed_paste_markers(input: &str) -> String {
    input.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "")
}

fn handle_command(state: &mut ReplState, line: &str) -> Result<ReplControl, String> {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or("");
    match command {
        ":help" => {
            print_help();
        }
        ":quit" | ":q" => return Ok(ReplControl::Exit),
        ":namespace" => {
            let Some(namespace) = parts.next() else {
                return Err("Usage: :namespace <name>".to_string());
            };
            state.namespace = namespace.to_string();
            state.symbol_revision_id = None;
            state.generation = None;
            state.revision_id = None;
            state.capabilities.clear();
            state.draft = None;
            println!("namespace = {}", state.namespace);
        }
        ":symbols" => {
            let generation = parts
                .next()
                .map(|value| value.parse::<i64>())
                .transpose()
                .map_err(|_| "Usage: :symbols [generation]".to_string())?;
            let mut path = format!("/symbols?namespace={}", state.namespace);
            if let Some(generation) = generation {
                path.push_str(&format!("&generation={generation}"));
            }
            let response = request_json(&state.server_url, "GET", &path, None)?;
            state.generation = response["generation"].as_i64();
            print_symbols(&response);
        }
        ":symbol" => {
            let kind = parts
                .next()
                .ok_or_else(|| "Usage: :symbol <function|type> <name>".to_string())?;
            if kind != "function" && kind != "type" {
                return Err("Symbol kind must be function or type".to_string());
            }
            let symbol = parts
                .next()
                .ok_or_else(|| "Usage: :symbol <function|type> <name>".to_string())?;
            if parts.next().is_some() {
                return Err("Usage: :symbol <function|type> <name>".to_string());
            }
            state.symbol_kind = kind.to_string();
            state.symbol = symbol.to_string();
            state.symbol_revision_id = None;
            state.draft = None;
            state.capabilities.clear();
            println!(
                "symbol = {}/{} (use :load for an existing symbol or :paste to create it)",
                state.symbol_kind, state.symbol
            );
        }
        ":load" => {
            load_symbol(state)?;
        }
        ":save" => {
            save_symbol(state)?;
        }
        ":legacy-changeset" => {
            let Some(changeset) = parts.next() else {
                println!(
                    "changeset = {} revision = {}",
                    state.changeset,
                    state.revision_id.as_deref().unwrap_or("unset")
                );
                return Ok(ReplControl::Continue);
            };
            state.changeset = changeset.to_string();
            state.revision_id = None;
            state.capabilities.clear();
            state.draft = None;
            println!("changeset = {} (use :load or :paste)", state.changeset);
        }
        ":legacy-changesets" => {
            let path = format!("/changesets?namespace={}", state.namespace);
            let response = request_json(&state.server_url, "GET", &path, None)?;
            print_changesets(&response);
        }
        ":resources" => {
            let path = format!("/resources?namespace={}", state.namespace);
            let response = request_json(&state.server_url, "GET", &path, None)?;
            print_resources(&response);
        }
        ":resource" => {
            let name = parts
                .next()
                .ok_or_else(|| "Usage: :resource <name>".to_string())?;
            let path = format!("/resources/{}/{}", state.namespace, name);
            let response = request_json(&state.server_url, "GET", &path, None)?;
            print_json_pretty(&response);
        }
        ":resource-put" => {
            let name = parts
                .next()
                .ok_or_else(|| "Usage: :resource-put <name> [kind]".to_string())?;
            let kind = parts.next().unwrap_or("secret/json");
            if parts.next().is_some() {
                return Err("Usage: :resource-put <name> [kind]".to_string());
            }
            println!("resource JSON; end with :end");
            let encoded = read_paste_mode()?;
            let value: Value = serde_json::from_str(&encoded)
                .map_err(|error| format!("Invalid resource JSON: {error}"))?;
            let list_path = format!("/resources?namespace={}", state.namespace);
            let listed = request_json(&state.server_url, "GET", &list_path, None)?;
            let expected_resource_id = listed["resources"]
                .as_array()
                .and_then(|resources| {
                    resources
                        .iter()
                        .find(|resource| resource["name"].as_str().is_some_and(|item| item == name))
                })
                .and_then(|resource| resource["resource_id"].as_str());
            let response = request_json(
                &state.server_url,
                "POST",
                "/resources/put",
                Some(json!({
                    "namespace": state.namespace,
                    "name": name,
                    "kind": kind,
                    "expected_resource_id": expected_resource_id,
                    "value": value,
                })),
            )?;
            print_json_pretty(&response);
        }
        ":resource-read" => {
            let name = parts
                .next()
                .ok_or_else(|| "Usage: :resource-read <name> [resource_id]".to_string())?;
            let resource_id = parts.next();
            if parts.next().is_some() {
                return Err("Usage: :resource-read <name> [resource_id]".to_string());
            }
            let response = request_json(
                &state.server_url,
                "POST",
                "/resources/read",
                Some(json!({
                    "namespace": state.namespace,
                    "name": name,
                    "resource_id": resource_id,
                })),
            )?;
            print_json_pretty(&response);
        }
        ":capabilities" => {
            if state.capabilities.is_empty() {
                println!("capabilities = []");
            } else {
                for capability in &state.capabilities {
                    println!("{}", capability);
                }
            }
        }
        ":grant" => {
            let capability = parts
                .next()
                .ok_or_else(|| "Usage: :grant <capability>".to_string())?;
            if parts.next().is_some() {
                return Err("Usage: :grant <capability>".to_string());
            }
            if !state.capabilities.iter().any(|item| item == capability) {
                state.capabilities.push(capability.to_string());
                state.capabilities.sort();
            }
            state.sandbox = true;
            println!("granted {} (takes effect on :save)", capability);
        }
        ":revoke" => {
            let capability = parts
                .next()
                .ok_or_else(|| "Usage: :revoke <capability>".to_string())?;
            if parts.next().is_some() {
                return Err("Usage: :revoke <capability>".to_string());
            }
            state.capabilities.retain(|item| item != capability);
            println!("revoked {} (takes effect on :save)", capability);
        }
        ":legacy-load" => {
            if let Some(changeset) = parts.next() {
                state.changeset = changeset.to_string();
            }
            load_changeset(state)?;
        }
        ":snapshot" => {
            let Some(snapshot_id) = parts.next() else {
                println!(
                    "snapshot = {}",
                    state
                        .snapshot_id
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unset".to_string())
                );
                return Ok(ReplControl::Continue);
            };
            let snapshot_id = snapshot_id
                .parse::<i64>()
                .map_err(|_| format!("Invalid snapshot id: {}", snapshot_id))?;
            state.snapshot_id = Some(snapshot_id);
            println!("snapshot = {}", snapshot_id);
        }
        ":snapshot-create" => {
            let generation = parts
                .next()
                .map(|value| value.parse::<i64>())
                .transpose()
                .map_err(|_| "Usage: :snapshot-create [generation]".to_string())?;
            let snapshot_id = create_snapshot(state, generation)?;
            println!("created snapshot {} for {}", snapshot_id, state.namespace);
        }
        ":namespaces" => {
            let response = request_json(&state.server_url, "GET", "/namespaces", None)?;
            print_namespaces(&response);
        }
        ":bindings" => {
            let path = format!("/namespaces/{}", state.namespace);
            let response = request_json(&state.server_url, "GET", &path, None)?;
            print_bindings(&response);
        }
        ":diff" => {
            let Some(from) = parts.next() else {
                return Err("Usage: :diff <from_generation> <to_generation>".to_string());
            };
            let Some(to) = parts.next() else {
                return Err("Usage: :diff <from_generation> <to_generation>".to_string());
            };
            let path = format!(
                "/namespaces/{}/diff?from={}&to={}",
                state.namespace, from, to
            );
            let response = request_json(&state.server_url, "GET", &path, None)?;
            print_diff(&response);
        }
        ":paste" => {
            let source = read_paste_mode()?;
            state.draft = Some(source);
            println!("stored draft");
        }
        ":show" => match &state.draft {
            Some(draft) => println!("{}", draft),
            None => println!("draft is empty"),
        },
        ":clear" => {
            state.draft = None;
            println!("draft cleared");
        }
        ":legacy-save" => {
            save_changeset(state)?;
        }
        ":legacy-edit" => {
            let start = parts
                .next()
                .ok_or_else(|| "Usage: :edit <start_byte> <end_byte>".to_string())?
                .parse::<usize>()
                .map_err(|_| "Edit start must be a byte offset".to_string())?;
            let end = parts
                .next()
                .ok_or_else(|| "Usage: :edit <start_byte> <end_byte>".to_string())?
                .parse::<usize>()
                .map_err(|_| "Edit end must be a byte offset".to_string())?;
            if parts.next().is_some() {
                return Err("Usage: :edit <start_byte> <end_byte>".to_string());
            }
            let revision_id = state
                .revision_id
                .as_deref()
                .ok_or_else(|| "No saved revision. Use :save first.".to_string())?;
            println!("replacement text; use an empty paste to delete the range");
            let replacement = read_paste_mode()?;
            let response = request_json(
                &state.server_url,
                "POST",
                "/changesets/edit",
                Some(json!({
                    "namespace": state.namespace,
                    "changeset": state.changeset,
                    "revision_id": revision_id,
                    "edits": [{
                        "start": start,
                        "end": end,
                        "replacement": replacement,
                    }],
                })),
            )?;
            let revision = &response["revision"];
            state.revision_id = Some(
                revision["revision_id"]
                    .as_str()
                    .ok_or_else(|| "Edit response missing revision_id".to_string())?
                    .to_string(),
            );
            state.draft = Some(
                revision["source"]
                    .as_str()
                    .ok_or_else(|| "Edit response missing source".to_string())?
                    .to_string(),
            );
            println!(
                "edited {}/{} revision {}",
                state.namespace,
                state.changeset,
                state.revision_id.as_deref().unwrap_or("?")
            );
        }
        ":legacy-compile" => {
            let revision_id = state
                .revision_id
                .as_deref()
                .ok_or_else(|| "No saved revision. Use :save first.".to_string())?;
            let response = request_json(
                &state.server_url,
                "POST",
                "/changesets/compile",
                Some(json!({
                    "namespace": state.namespace,
                    "changeset": state.changeset,
                    "revision_id": revision_id,
                })),
            )?;
            print_compile_response(&response);
        }
        ":legacy-publish" => {
            let revision_id = state
                .revision_id
                .as_deref()
                .ok_or_else(|| "No saved revision. Use :save first.".to_string())?;
            let symbols: Vec<String> = parts.map(|item| item.to_string()).collect();
            let body = json!({
                "namespace": state.namespace,
                "changeset": state.changeset,
                "revision_id": revision_id,
                "symbols": if symbols.is_empty() { Value::Null } else { json!(symbols) },
            });
            let response =
                request_json(&state.server_url, "POST", "/changesets/publish", Some(body))?;
            let generation = response["generation"]
                .as_i64()
                .ok_or_else(|| "Publish response missing generation".to_string())?;
            println!("published {} generation {}", state.namespace, generation);
            if let Some(published_symbols) = response["published_symbols"].as_array() {
                for symbol in published_symbols {
                    println!(
                        "  {} -> {}",
                        symbol["symbol"].as_str().unwrap_or("?"),
                        symbol["artifact_hash"].as_str().unwrap_or("?")
                    );
                }
            }
            let snapshot_id = response["snapshot_id"]
                .as_i64()
                .ok_or_else(|| "Publish response missing snapshot_id".to_string())?;
            state.snapshot_id = Some(snapshot_id);
            println!("current snapshot = {}", snapshot_id);
        }
        ":run" => {
            let snapshot_id = state.snapshot_id.ok_or_else(|| {
                "No snapshot selected. Use :snapshot or :snapshot-create.".to_string()
            })?;
            let Some(target) = parts.next() else {
                return Err("Usage: :run <symbol|hash> [args ...]".to_string());
            };
            let args: Vec<String> = parts.map(|item| item.to_string()).collect();
            let response = request_json(
                &state.server_url,
                "POST",
                "/run",
                Some(json!({
                    "snapshot_id": snapshot_id,
                    "target": target,
                    "args": args,
                    "limits": {
                        "timeout_ms": state.timeout_ms,
                        "output_limit_bytes": state.output_limit_bytes,
                    }
                })),
            )?;
            print_run_response(&response);
        }
        ":sandbox" => {
            let Some(value) = parts.next() else {
                println!("sandbox = {}", state.sandbox);
                return Ok(ReplControl::Continue);
            };
            match value {
                "on" => state.sandbox = true,
                "off" => state.sandbox = false,
                _ => return Err("Usage: :sandbox on|off".to_string()),
            }
            println!("sandbox = {}", state.sandbox);
        }
        ":limits" => {
            let first = parts.next();
            let second = parts.next();
            match (first, second) {
                (None, None) => {
                    println!(
                        "limits timeout_ms={} output_limit_bytes={}",
                        state.timeout_ms, state.output_limit_bytes
                    );
                }
                (Some(timeout_ms), Some(output_limit_bytes)) => {
                    state.timeout_ms = timeout_ms
                        .parse::<u64>()
                        .map_err(|_| "Invalid timeout_ms".to_string())?;
                    state.output_limit_bytes = output_limit_bytes
                        .parse::<usize>()
                        .map_err(|_| "Invalid output_limit_bytes".to_string())?;
                    println!(
                        "limits timeout_ms={} output_limit_bytes={}",
                        state.timeout_ms, state.output_limit_bytes
                    );
                }
                _ => return Err("Usage: :limits <timeout_ms> <output_limit_bytes>".to_string()),
            }
        }
        _ => return Err(format!("Unknown command: {}", command)),
    }
    Ok(ReplControl::Continue)
}

fn create_snapshot(state: &mut ReplState, generation: Option<i64>) -> Result<i64, String> {
    let response = request_json(
        &state.server_url,
        "POST",
        "/snapshot",
        Some(json!({
            "namespace": state.namespace,
            "generation": generation,
        })),
    )?;
    let snapshot_id = response["snapshot_id"]
        .as_i64()
        .ok_or_else(|| "Snapshot response missing snapshot_id".to_string())?;
    state.snapshot_id = Some(snapshot_id);
    Ok(snapshot_id)
}

fn eval_expression(state: &ReplState, expr: &str) -> Result<(), String> {
    let snapshot_id = state.snapshot_id.ok_or_else(|| {
        "No snapshot selected. Use :snapshot-create or :snapshot before evaluating expressions."
            .to_string()
    })?;
    let source = format!("fn main() {{ {} }}", expr);
    let response = request_json(
        &state.server_url,
        "POST",
        "/eval",
        Some(json!({
            "snapshot_id": snapshot_id,
            "sandbox": state.sandbox,
            "source": source,
            "limits": {
                "timeout_ms": state.timeout_ms,
                "output_limit_bytes": state.output_limit_bytes,
            }
        })),
    )?;
    if let Some(stdout) = response["stdout"].as_str() {
        println!("{}", stdout);
        if let Some(beam_time_us) = response["beam_time_us"].as_u64() {
            println!("beam_time_us={}", beam_time_us);
        }
    } else {
        print_json_pretty(&response);
    }
    Ok(())
}

fn ensure_service_available(server_url: &str) -> Result<(), String> {
    let response = request_json(server_url, "GET", "/health", None)?;
    match response["status"].as_str() {
        Some("ok") => Ok(()),
        _ => Err(format!(
            "Service at {} is not healthy",
            server_url.trim_end_matches('/')
        )),
    }
}

fn load_symbol(state: &mut ReplState) -> Result<(), String> {
    state.generation = Some(current_namespace_generation(state)?);
    let path = format!(
        "/symbol?namespace={}&kind={}&symbol={}",
        state.namespace, state.symbol_kind, state.symbol
    );
    let response = request_json(&state.server_url, "GET", &path, None)?;
    let symbol = &response["symbol"];
    state.symbol_revision_id = Some(
        symbol["revision_id"]
            .as_str()
            .ok_or_else(|| "Symbol response missing revision_id".to_string())?
            .to_string(),
    );
    state.draft = Some(
        symbol["source"]
            .as_str()
            .ok_or_else(|| "Symbol response missing source".to_string())?
            .to_string(),
    );
    state.sandbox = symbol["sandbox"].as_bool().unwrap_or(true);
    state.capabilities = symbol["capabilities"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    println!(
        "loaded {}/{}/{} revision {}",
        state.namespace,
        state.symbol_kind,
        state.symbol,
        state.symbol_revision_id.as_deref().unwrap_or("?")
    );
    Ok(())
}

fn save_symbol(state: &mut ReplState) -> Result<(), String> {
    let source = state
        .draft
        .as_deref()
        .ok_or_else(|| "No symbol draft. Use :paste first.".to_string())?;
    let expected_generation = match state.generation {
        Some(generation) => generation,
        None => current_namespace_generation(state)?,
    };
    let response = request_json(
        &state.server_url,
        "POST",
        "/symbols/put",
        Some(json!({
            "namespace": state.namespace,
            "kind": state.symbol_kind,
            "symbol": state.symbol,
            "source": source,
            "expected_generation": expected_generation,
            "expected_revision_id": state.symbol_revision_id,
            "sandbox": state.sandbox,
            "capabilities": state.capabilities,
        })),
    )?;
    state.generation = response["generation"].as_i64();
    state.snapshot_id = response["snapshot_id"].as_i64();
    state.symbol_revision_id = response["symbol"]["revision_id"]
        .as_str()
        .map(str::to_string);
    println!(
        "published {}/{}/{} generation={} revision={} snapshot={}",
        state.namespace,
        state.symbol_kind,
        state.symbol,
        state.generation.unwrap_or(0),
        state.symbol_revision_id.as_deref().unwrap_or("?"),
        state.snapshot_id.unwrap_or(0),
    );
    Ok(())
}

fn current_namespace_generation(state: &ReplState) -> Result<i64, String> {
    let response = request_json(&state.server_url, "GET", "/namespaces", None)?;
    Ok(response["namespaces"]
        .as_array()
        .and_then(|namespaces| {
            namespaces.iter().find(|namespace| {
                namespace["name"]
                    .as_str()
                    .is_some_and(|name| name == state.namespace)
            })
        })
        .and_then(|namespace| namespace["current_generation"].as_i64())
        .unwrap_or(0))
}

fn load_changeset(state: &mut ReplState) -> Result<(), String> {
    let path = format!("/changesets/{}/{}", state.namespace, state.changeset);
    let response = request_json(&state.server_url, "GET", &path, None)?;
    let revision_id = response["revision"]["revision_id"]
        .as_str()
        .ok_or_else(|| "Changeset response missing revision_id".to_string())?;
    let source = response["revision"]["source"]
        .as_str()
        .ok_or_else(|| "Changeset response missing source".to_string())?;
    state.revision_id = Some(revision_id.to_string());
    state.draft = Some(source.to_string());
    state.snapshot_id = response["revision"]["base_snapshot_id"].as_i64();
    state.sandbox = response["revision"]["sandbox"].as_bool().unwrap_or(false);
    state.capabilities = response["revision"]["capabilities"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    println!(
        "loaded {}/{} revision {}",
        state.namespace, state.changeset, revision_id
    );
    Ok(())
}

fn save_changeset(state: &mut ReplState) -> Result<(), String> {
    let source = state
        .draft
        .as_deref()
        .ok_or_else(|| "No draft. Use :paste first.".to_string())?;
    let response = request_json(
        &state.server_url,
        "POST",
        "/changesets/save",
        Some(json!({
            "namespace": state.namespace,
            "changeset": state.changeset,
            "source": source,
            "base_snapshot_id": state.snapshot_id,
            "sandbox": state.sandbox,
            "capabilities": state.capabilities,
            "expected_parent_revision_id": state.revision_id,
        })),
    )?;
    let revision_id = response["revision"]["revision_id"]
        .as_str()
        .ok_or_else(|| "Save response missing revision_id".to_string())?
        .to_string();
    state.revision_id = Some(revision_id.clone());
    println!(
        "saved {}/{} revision {}",
        state.namespace, state.changeset, revision_id
    );
    Ok(())
}

fn request_json(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{}", server_url.trim_end_matches('/'), path);
    let response = match method {
        "GET" => ureq::get(&url).call(),
        "POST" => {
            let Some(body) = body else {
                return Err("POST request missing body".to_string());
            };
            ureq::post(&url).send_json(body)
        }
        _ => return Err(format!("Unsupported HTTP method: {}", method)),
    };

    match response {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|err| format!("Invalid JSON response: {}", err)),
        Err(ureq::Error::Status(_, response)) => {
            let value = response
                .into_json::<Value>()
                .unwrap_or_else(|_| json!({ "error": "request failed" }));
            let message = value["error"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            Err(message)
        }
        Err(err) => Err(format!("Request failed: {}", err)),
    }
}

fn read_paste_mode() -> Result<String, String> {
    println!("paste mode; end with :end");
    let stdin = io::stdin();
    let mut lines = Vec::new();
    loop {
        print!("... ");
        io::stdout().flush().map_err(|err| err.to_string())?;
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .map_err(|err| format!("Failed to read input: {}", err))?;
        let trimmed = line.trim_end();
        if trimmed == ":end" {
            break;
        }
        lines.push(trimmed.to_string());
    }
    Ok(lines.join("\n"))
}

fn print_prompt(state: &ReplState) {
    let snapshot = state
        .snapshot_id
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    print!(
        "lux[{}/{}:{}#{}@{}]> ",
        state.namespace,
        state.symbol_kind,
        state.symbol,
        state
            .generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
        snapshot
    );
    let _ = io::stdout().flush();
}

fn print_help() {
    println!("Commands:");
    println!("  :help");
    println!("  :namespace <name>");
    println!("  :symbols [generation]");
    println!("  :symbol <function|type> <name>");
    println!("  :load");
    println!("  :paste");
    println!("  :show");
    println!("  :clear");
    println!("  :save  (atomic compile + publish + snapshot of the selected symbol)");
    println!("  :resources");
    println!("  :resource <name>");
    println!("  :resource-put <name> [kind]");
    println!("  :resource-read <name> [resource_id]");
    println!("  :capabilities");
    println!("  :grant <capability>");
    println!("  :revoke <capability>");
    println!("  :snapshot [id]");
    println!("  :snapshot-create [generation]");
    println!("  :namespaces");
    println!("  :bindings");
    println!("  :diff <from_generation> <to_generation>");
    println!("  :run <symbol|hash> [args ...]");
    println!("  :sandbox on|off");
    println!("  :limits [timeout_ms output_limit_bytes]");
    println!("  :quit");
    println!("Legacy changeset compatibility commands use the :legacy-* prefix.");
    println!("Bare lines are evaluated as expressions via fn main() {{ ... }}.");
}

fn print_symbols(response: &Value) {
    println!(
        "namespace={} generation={}",
        response["namespace"].as_str().unwrap_or("?"),
        response["generation"].as_i64().unwrap_or(0),
    );
    let Some(symbols) = response["symbols"].as_array() else {
        print_json_pretty(response);
        return;
    };
    for symbol in symbols {
        println!(
            "  {}/{} revision={} artifact={}",
            symbol["kind"].as_str().unwrap_or("?"),
            symbol["symbol"].as_str().unwrap_or("?"),
            symbol["revision_id"].as_str().unwrap_or("?"),
            symbol["artifact_hash"].as_str().unwrap_or("-"),
        );
    }
    if symbols.is_empty() {
        println!("  (none)");
    }
}

fn print_changesets(response: &Value) {
    let Some(changesets) = response["changesets"].as_array() else {
        print_json_pretty(response);
        return;
    };
    for changeset in changesets {
        println!(
            "{}/{} head={} revisions={}",
            changeset["namespace"].as_str().unwrap_or("?"),
            changeset["name"].as_str().unwrap_or("?"),
            changeset["head_revision_id"].as_str().unwrap_or("-"),
            changeset["revision_count"].as_i64().unwrap_or(0),
        );
    }
}

fn print_namespaces(response: &Value) {
    let Some(namespaces) = response["namespaces"].as_array() else {
        print_json_pretty(response);
        return;
    };
    for namespace in namespaces {
        println!(
            "{} generation={} bindings={} snapshots={}",
            namespace["name"].as_str().unwrap_or("?"),
            namespace["current_generation"]
                .as_i64()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            namespace["binding_count"].as_i64().unwrap_or(0),
            namespace["snapshot_count"].as_i64().unwrap_or(0)
        );
    }
}

fn print_resources(response: &Value) {
    let Some(resources) = response["resources"].as_array() else {
        print_json_pretty(response);
        return;
    };
    for resource in resources {
        println!(
            "{}/{} kind={} head={} revision={}",
            resource["namespace"].as_str().unwrap_or("?"),
            resource["name"].as_str().unwrap_or("?"),
            resource["kind"].as_str().unwrap_or("?"),
            resource["resource_id"].as_str().unwrap_or("?"),
            resource["revision_id"].as_str().unwrap_or("-"),
        );
    }
    if resources.is_empty() {
        println!("(none)");
    }
}

fn print_bindings(response: &Value) {
    println!(
        "namespace={} generation={}",
        response["namespace"].as_str().unwrap_or("?"),
        response["generation"].as_i64().unwrap_or(0)
    );
    if let Some(bindings) = response["bindings"].as_array() {
        for binding in bindings {
            println!(
                "  {} -> {}",
                binding["symbol"].as_str().unwrap_or("?"),
                binding["artifact_hash"].as_str().unwrap_or("?")
            );
        }
    }
}

fn print_diff(response: &Value) {
    println!(
        "{} {} -> {}",
        response["namespace"].as_str().unwrap_or("?"),
        response["from_generation"].as_i64().unwrap_or(0),
        response["to_generation"].as_i64().unwrap_or(0)
    );
    print_change_section("added", &response["added"]);
    print_change_section("removed", &response["removed"]);
    print_change_section("changed", &response["changed"]);
}

fn print_change_section(label: &str, value: &Value) {
    let Some(items) = value.as_array() else {
        return;
    };
    println!("{}:", label);
    for item in items {
        println!(
            "  {} {} -> {}",
            item["symbol"].as_str().unwrap_or("?"),
            item["from_artifact_hash"].as_str().unwrap_or("-"),
            item["to_artifact_hash"].as_str().unwrap_or("-")
        );
    }
    if items.is_empty() {
        println!("  (none)");
    }
}

fn print_compile_response(response: &Value) {
    if let Some(entry) = response["entry"].as_object() {
        println!(
            "entry {} -> {}",
            entry["function"].as_str().unwrap_or("apply"),
            entry["artifact_hash"].as_str().unwrap_or("?")
        );
    } else {
        println!("entry none");
    }
    if let Some(artifacts) = response["artifacts"].as_array() {
        for artifact in artifacts {
            println!(
                "  {} -> {} deps={}",
                artifact["source_name"].as_str().unwrap_or("?"),
                artifact["artifact_hash"].as_str().unwrap_or("?"),
                artifact["dependencies"]
                    .as_array()
                    .map(|deps| deps.len().to_string())
                    .unwrap_or_else(|| "0".to_string())
            );
        }
    }
}

fn print_run_response(response: &Value) {
    if let Some(stdout) = response["stdout"].as_str() {
        println!("{}", stdout);
        if let Some(beam_time_us) = response["beam_time_us"].as_u64() {
            println!("beam_time_us={}", beam_time_us);
        }
    } else {
        print_json_pretty(response);
    }
}

fn print_json_pretty(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(rendered) => println!("{}", rendered),
        Err(_) => println!("{}", value),
    }
}
