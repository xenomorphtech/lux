use std::env;
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    LATEST_PROTOCOL_VERSION,
];

fn main() {
    let server_url = parse_server_url();
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("Lux MCP input error: {error}");
                break;
            }
        };
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = rpc_error(Value::Null, -32700, format!("Parse error: {error}"));
                if write_message(&mut stdout, &response).is_err() {
                    break;
                }
                continue;
            }
        };
        let Some(response) = handle_request(&server_url, &request) else {
            continue;
        };
        if let Err(error) = write_message(&mut stdout, &response) {
            eprintln!("Lux MCP output error: {error}");
            break;
        }
    }
}

fn parse_server_url() -> String {
    let mut explicit = None;
    let mut port = 4002_u16;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => {
                explicit = args.next();
                if explicit.is_none() {
                    eprintln!("Missing value for --server");
                    std::process::exit(2);
                }
            }
            "--port" => {
                let Some(value) = args.next() else {
                    eprintln!("Missing value for --port");
                    std::process::exit(2);
                };
                port = value.parse().unwrap_or_else(|_| {
                    eprintln!("Invalid port: {value}");
                    std::process::exit(2);
                });
            }
            _ => {
                eprintln!("Unknown option: {arg}");
                std::process::exit(2);
            }
        }
    }
    explicit.unwrap_or_else(|| format!("http://127.0.0.1:{port}"))
}

fn write_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn handle_request(server_url: &str, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => {
            let requested = request["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(LATEST_PROTOCOL_VERSION);
            let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                LATEST_PROTOCOL_VERSION
            };
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "lux", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Develop Lux projects through atomic namespace function/type symbols, immutable revisions, generations, snapshots, and protected resource metadata. Changesets are provenance/import compatibility, not the normal editing unit. Secret reveal is intentionally not exposed through MCP."
                }),
            ))
        }
        "ping" => Some(rpc_result(id, json!({}))),
        "tools/list" => Some(rpc_result(id, json!({"tools": tool_definitions()}))),
        "tools/call" => {
            let Some(name) = request["params"]["name"].as_str() else {
                return Some(rpc_error(id, -32602, "Missing tool name"));
            };
            let arguments = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Some(rpc_result(id, call_tool(server_url, name, arguments)))
        }
        _ => Some(rpc_error(id, -32601, format!("Method not found: {method}"))),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "lux.list_namespaces",
            "List namespace generations, binding counts, and snapshots.",
            object_schema(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "lux.get_namespace",
            "Read bindings in a namespace generation.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "generation": {"type": "integer", "description": "Optional generation"}
                }),
                &["namespace"],
            ),
            true,
            false,
        ),
        tool(
            "lux.list_symbols",
            "List atomic function and type symbols published in a namespace generation.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "generation": {"type": "integer", "minimum": 0, "description": "Optional generation; defaults to current"}
                }),
                &["namespace"],
            ),
            true,
            false,
        ),
        tool(
            "lux.get_symbol",
            "Read one immutable namespace function/type symbol revision, including its Lux source.",
            symbol_selector_schema(true),
            true,
            false,
        ),
        tool(
            "lux.put_symbol",
            "Atomically create or replace one namespace function/type symbol, compile it, publish a new generation, and create a snapshot. Replacements require the current revision ID.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "kind": {"type": "string", "enum": ["function", "type"]},
                    "symbol": string_property("Function or type name"),
                    "source": string_property("Exactly one complete Lux function or type definition"),
                    "expected_generation": {"type": "integer", "minimum": 0},
                    "expected_revision_id": string_property("Required when replacing; omit only when creating a symbol"),
                    "sandbox": {"type": "boolean", "description": "Optional; inherited on replacement, true on creation"},
                    "capabilities": {"type": "array", "items": {"type": "string"}, "description": "Optional; inherited on replacement"}
                }),
                &[
                    "namespace",
                    "kind",
                    "symbol",
                    "source",
                    "expected_generation",
                ],
            ),
            false,
            false,
        ),
        tool(
            "lux.diff_namespace",
            "Compare two immutable generations of a namespace.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "from_generation": {"type": "integer", "minimum": 1},
                    "to_generation": {"type": "integer", "minimum": 1}
                }),
                &["namespace", "from_generation", "to_generation"],
            ),
            true,
            false,
        ),
        tool(
            "lux.get_snapshot",
            "Read an immutable snapshot and its binding provenance.",
            object_schema(
                json!({"snapshot_id": {"type": "integer", "minimum": 1}}),
                &["snapshot_id"],
            ),
            true,
            false,
        ),
        tool(
            "lux.create_snapshot",
            "Freeze a namespace generation as an immutable execution snapshot.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "generation": {"type": "integer", "minimum": 1, "description": "Optional generation; defaults to the current generation"}
                }),
                &["namespace"],
            ),
            false,
            false,
        ),
        tool(
            "lux.list_changesets",
            "List database-native changesets and their head revisions.",
            object_schema(
                json!({"namespace": string_property("Namespace name")}),
                &["namespace"],
            ),
            true,
            false,
        ),
        tool(
            "lux.get_changeset",
            "Read a changeset revision, including its Lux source and capability grants.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "changeset": string_property("Changeset name"),
                    "revision_id": string_property("Optional immutable revision ID")
                }),
                &["namespace", "changeset"],
            ),
            true,
            false,
        ),
        tool(
            "lux.run",
            "Run a symbol or artifact from an immutable snapshot.",
            object_schema(
                json!({
                    "snapshot_id": {"type": "integer", "minimum": 1},
                    "target": string_property("Symbol name or artifact hash"),
                    "args": {"type": "array", "items": {"type": "string"}, "default": []},
                    "limits": {
                        "type": "object",
                        "properties": {
                            "timeout_ms": {"type": "integer", "minimum": 1},
                            "output_limit_bytes": {"type": "integer", "minimum": 1}
                        },
                        "additionalProperties": false
                    }
                }),
                &["snapshot_id", "target"],
            ),
            false,
            false,
        ),
        tool(
            "lux.list_resources",
            "List protected namespace resource metadata. Values, nonces, and ciphertext are never returned.",
            object_schema(
                json!({"namespace": string_property("Namespace name")}),
                &["namespace"],
            ),
            true,
            false,
        ),
        tool(
            "lux.get_resource",
            "Read protected resource metadata for the head or an immutable resource version. Secret material is never returned.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "name": string_property("Resource name"),
                    "resource_id": string_property("Optional immutable resource ID")
                }),
                &["namespace", "name"],
            ),
            true,
            false,
        ),
        tool(
            "lux.put_resource",
            "Create or update an encrypted namespace resource. The response contains metadata only.",
            object_schema(
                json!({
                    "namespace": string_property("Namespace name"),
                    "name": string_property("Resource name"),
                    "kind": {"type": "string", "default": "secret/json"},
                    "revision_id": string_property("Optional owning source revision"),
                    "expected_resource_id": string_property("Current resource head ID for updates"),
                    "value": {"description": "JSON value to encrypt and store"}
                }),
                &["namespace", "name", "value"],
            ),
            false,
            false,
        ),
    ]
}

fn tool(
    name: &str,
    description: &str,
    input_schema: Value,
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": read_only
        }
    })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn string_property(description: &str) -> Value {
    json!({"type": "string", "minLength": 1, "description": description})
}

fn symbol_selector_schema(with_generation: bool) -> Value {
    let mut properties = serde_json::Map::from_iter([
        ("namespace".to_string(), string_property("Namespace name")),
        (
            "kind".to_string(),
            json!({"type": "string", "enum": ["function", "type"]}),
        ),
        (
            "symbol".to_string(),
            string_property("Function or type name"),
        ),
    ]);
    if with_generation {
        properties.insert(
            "generation".to_string(),
            json!({"type": "integer", "minimum": 0}),
        );
    }
    object_schema(Value::Object(properties), &["namespace", "kind", "symbol"])
}

fn call_tool(server_url: &str, name: &str, arguments: Value) -> Value {
    match try_call_tool(server_url, name, arguments) {
        Ok(value) => tool_result(value, false),
        Err(error) => tool_result(json!({"error": error}), true),
    }
}

fn try_call_tool(server_url: &str, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "lux.list_namespaces" => api_request(server_url, "GET", "/namespaces", None),
        "lux.get_namespace" => {
            let namespace = required_string(&arguments, "namespace")?;
            let mut path = format!("/namespaces/{namespace}");
            if let Some(generation) = arguments.get("generation").and_then(Value::as_i64) {
                path.push_str(&format!("?generation={generation}"));
            }
            api_request(server_url, "GET", &path, None)
        }
        "lux.list_symbols" => {
            let namespace = required_string(&arguments, "namespace")?;
            let mut path = format!("/symbols?namespace={namespace}");
            if let Some(generation) = arguments.get("generation").and_then(Value::as_i64) {
                path.push_str(&format!("&generation={generation}"));
            }
            api_request(server_url, "GET", &path, None)
        }
        "lux.get_symbol" => {
            let namespace = required_string(&arguments, "namespace")?;
            let kind = required_string(&arguments, "kind")?;
            let symbol = required_string(&arguments, "symbol")?;
            let mut path = format!("/symbol?namespace={namespace}&kind={kind}&symbol={symbol}");
            if let Some(generation) = arguments.get("generation").and_then(Value::as_i64) {
                path.push_str(&format!("&generation={generation}"));
            }
            api_request(server_url, "GET", &path, None)
        }
        "lux.put_symbol" => api_request(server_url, "POST", "/symbols/put", Some(arguments)),
        "lux.diff_namespace" => {
            let namespace = required_string(&arguments, "namespace")?;
            let from_generation = required_i64(&arguments, "from_generation")?;
            let to_generation = required_i64(&arguments, "to_generation")?;
            api_request(
                server_url,
                "GET",
                &format!("/namespaces/{namespace}/diff?from={from_generation}&to={to_generation}"),
                None,
            )
        }
        "lux.get_snapshot" => {
            let snapshot_id = required_i64(&arguments, "snapshot_id")?;
            api_request(
                server_url,
                "GET",
                &format!("/snapshots/{snapshot_id}"),
                None,
            )
        }
        "lux.create_snapshot" => api_request(server_url, "POST", "/snapshot", Some(arguments)),
        "lux.list_changesets" => {
            let namespace = required_string(&arguments, "namespace")?;
            api_request(
                server_url,
                "GET",
                &format!("/changesets?namespace={namespace}"),
                None,
            )
        }
        "lux.get_changeset" => {
            let namespace = required_string(&arguments, "namespace")?;
            let changeset = required_string(&arguments, "changeset")?;
            let mut path = format!("/changesets/{namespace}/{changeset}");
            if let Some(revision_id) = arguments.get("revision_id").and_then(Value::as_str) {
                path.push_str(&format!("?revision_id={revision_id}"));
            }
            api_request(server_url, "GET", &path, None)
        }
        "lux.run" => api_request(server_url, "POST", "/run", Some(arguments)),
        "lux.list_resources" => {
            let namespace = required_string(&arguments, "namespace")?;
            api_request(
                server_url,
                "GET",
                &format!("/resources?namespace={namespace}"),
                None,
            )
        }
        "lux.get_resource" => {
            let namespace = required_string(&arguments, "namespace")?;
            let name = required_string(&arguments, "name")?;
            let mut path = format!("/resources/{namespace}/{name}");
            if let Some(resource_id) = arguments.get("resource_id").and_then(Value::as_str) {
                path.push_str(&format!("?resource_id={resource_id}"));
            }
            api_request(server_url, "GET", &path, None)
        }
        "lux.put_resource" => api_request(server_url, "POST", "/resources/put", Some(arguments)),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing or invalid argument: {name}"))
}

fn required_i64(arguments: &Value, name: &str) -> Result<i64, String> {
    arguments
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Missing or invalid argument: {name}"))
}

fn api_request(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{}", server_url.trim_end_matches('/'), path);
    let response = match method {
        "GET" => ureq::get(&url).call(),
        "POST" => ureq::post(&url).send_json(body.unwrap_or_else(|| json!({}))),
        _ => return Err(format!("Unsupported HTTP method: {method}")),
    };
    match response {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| format!("Lux returned invalid JSON: {error}")),
        Err(ureq::Error::Status(status, response)) => {
            let value = response
                .into_json::<Value>()
                .unwrap_or_else(|_| json!({"error": format!("HTTP {status}")}));
            Err(value["error"]
                .as_str()
                .unwrap_or("Lux API request failed")
                .to_string())
        }
        Err(error) => Err(format!("Lux API request failed: {error}")),
    }
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": is_error
    })
}

#[cfg(test)]
mod tests {
    use super::{call_tool, handle_request, tool_definitions};
    use serde_json::json;

    #[test]
    fn initialize_negotiates_a_supported_protocol() {
        let response = handle_request(
            "http://127.0.0.1:1",
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"}
            }),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
    }

    #[test]
    fn tools_do_not_expose_secret_reveal() {
        let names = tool_definitions()
            .into_iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"lux.list_symbols".to_string()));
        assert!(names.contains(&"lux.get_symbol".to_string()));
        assert!(names.contains(&"lux.put_symbol".to_string()));
        assert!(!names.contains(&"lux.edit_changeset".to_string()));
        assert!(names.contains(&"lux.diff_namespace".to_string()));
        assert!(names.contains(&"lux.put_resource".to_string()));
        assert!(names.contains(&"lux.get_resource".to_string()));
        assert!(!names.iter().any(|name| name.contains("read_resource")));
    }

    #[test]
    fn invalid_tool_arguments_are_reported_as_tool_errors() {
        let result = call_tool("http://127.0.0.1:1", "lux.get_namespace", json!({}));
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["error"],
            "Missing or invalid argument: namespace"
        );
    }
}
