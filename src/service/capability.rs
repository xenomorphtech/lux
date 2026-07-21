use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{Value, json};

use super::ServiceError;

pub const TEST_ECHO_CAPABILITY: &str = "lux.test.echo";
pub const RESOURCE_HEAD_CAPABILITY: &str = "resource.head";
pub const RESOURCE_READ_CAPABILITY: &str = "resource.read";
pub const RESOURCE_WRITE_CAPABILITY: &str = "resource.write";

const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const BRIDGE_SOURCE: &str = r#"-module(lux_capability).
-export([
    set_grants/1,
    invoke/2,
    udp_open/0,
    udp_send/4,
    udp_recv/2,
    udp_recv_optional/2,
    udp_close/1,
    tcp_request/4,
    random_bytes/1,
    sha256/1,
    mod_pow/3,
    aes_256_cbc/4,
    wall_time_ms/0,
    sleep_ms/1,
    resource_head/2,
    resource_read/2,
    resource_read/3,
    resource_write/3,
    resource_write/5
]).

set_grants(Grants) when is_list(Grants) ->
    put({lux_capability, grants}, Grants),
    ok.

require(Capability) ->
    Grants = case get({lux_capability, grants}) of
        undefined -> [];
        Value -> Value
    end,
    case lists:member(Capability, Grants) of
        true -> ok;
        false -> erlang:error({capability_not_granted, Capability})
    end.

require_resource(Action, Namespace, Name) ->
    Grants = case get({lux_capability, grants}) of
        undefined -> [];
        Value -> Value
    end,
    Scoped = <<Action/binary, "/", Namespace/binary, "/", Name/binary>>,
    case lists:member(Action, Grants) orelse lists:member(Scoped, Grants) of
        true -> ok;
        false -> erlang:error({capability_not_granted, Scoped})
    end.

next_handle() ->
    Handle = case get({lux_capability, next_handle}) of
        undefined -> 1;
        Value -> Value
    end,
    put({lux_capability, next_handle}, Handle + 1),
    Handle.

udp_socket(Handle) ->
    case get({lux_capability, udp, Handle}) of
        undefined -> erlang:error({invalid_udp_handle, Handle});
        Socket -> Socket
    end.

udp_open() ->
    require(<<"net.udp">>),
    case gen_udp:open(0, [binary, {active, false}]) of
        {ok, Socket} ->
            Handle = next_handle(),
            put({lux_capability, udp, Handle}, Socket),
            Handle;
        {error, Reason} -> erlang:error({udp_open_failed, Reason})
    end.

udp_send(Handle, Host, Port, Data)
        when is_integer(Handle), is_binary(Host), is_integer(Port), is_binary(Data) ->
    require(<<"net.udp">>),
    Socket = udp_socket(Handle),
    case inet:getaddr(binary_to_list(Host), inet) of
        {ok, Address} ->
            case gen_udp:send(Socket, Address, Port, Data) of
                ok -> byte_size(Data);
                {error, Reason} -> erlang:error({udp_send_failed, Reason})
            end;
        {error, Reason} -> erlang:error({host_resolution_failed, Reason})
    end.

udp_recv(Handle, Timeout) when is_integer(Handle), is_integer(Timeout) ->
    require(<<"net.udp">>),
    Socket = udp_socket(Handle),
    case gen_udp:recv(Socket, 0, Timeout) of
        {ok, {_Address, _Port, Data}} -> Data;
        {error, Reason} -> erlang:error({udp_recv_failed, Reason})
    end.

udp_recv_optional(Handle, Timeout) when is_integer(Handle), is_integer(Timeout) ->
    require(<<"net.udp">>),
    Socket = udp_socket(Handle),
    case gen_udp:recv(Socket, 0, Timeout) of
        {ok, {_Address, _Port, Data}} -> Data;
        {error, timeout} -> <<>>;
        {error, Reason} -> erlang:error({udp_recv_failed, Reason})
    end.

udp_close(Handle) when is_integer(Handle) ->
    require(<<"net.udp">>),
    Socket = udp_socket(Handle),
    erase({lux_capability, udp, Handle}),
    gen_udp:close(Socket).

tcp_request(Host, Port, Data, Timeout)
        when is_binary(Host), is_integer(Port), is_binary(Data), is_integer(Timeout) ->
    require(<<"net.tcp">>),
    case gen_tcp:connect(binary_to_list(Host), Port, [binary, {active, false}], Timeout) of
        {ok, Socket} ->
            try
                ok = gen_tcp:send(Socket, Data),
                case gen_tcp:recv(Socket, 0, Timeout) of
                    {ok, Response} -> Response;
                    {error, Reason} -> erlang:error({tcp_recv_failed, Reason})
                end
            after
                gen_tcp:close(Socket)
            end;
        {error, Reason} -> erlang:error({tcp_connect_failed, Reason})
    end.

random_bytes(Length) when is_integer(Length), Length >= 0, Length =< 1048576 ->
    require(<<"crypto.random">>),
    crypto:strong_rand_bytes(Length).

sha256(Data) when is_binary(Data) ->
    require(<<"crypto.hash">>),
    crypto:hash(sha256, Data).

mod_pow(Base, Exponent, Modulus)
        when is_binary(Base), is_binary(Exponent), is_binary(Modulus) ->
    require(<<"crypto.modular">>),
    crypto:mod_pow(Base, Exponent, Modulus).

aes_256_cbc(Data, Key, Iv, Encrypt)
        when is_binary(Data), is_binary(Key), is_binary(Iv), is_boolean(Encrypt) ->
    require(<<"crypto.block">>),
    crypto:crypto_one_time(aes_256_cbc, Key, Iv, Data, Encrypt).

wall_time_ms() ->
    require(<<"clock.wall">>),
    erlang:system_time(millisecond).

sleep_ms(Duration) when is_integer(Duration), Duration >= 0, Duration =< 600000 ->
    require(<<"clock.sleep">>),
    timer:sleep(Duration).

resource_head(Namespace, Name) when is_binary(Namespace), is_binary(Name) ->
    require_resource(<<"resource.read">>, Namespace, Name),
    Request = <<(byte_size(Namespace)):32, Namespace/binary,
                (byte_size(Name)):32, Name/binary,
                0:32>>,
    invoke_host(<<"resource.head">>, Request).

resource_read(Namespace, Name) ->
    resource_read(Namespace, Name, <<>>).

resource_read(Namespace, Name, ResourceId)
        when is_binary(Namespace), is_binary(Name), is_binary(ResourceId) ->
    require_resource(<<"resource.read">>, Namespace, Name),
    Request = <<(byte_size(Namespace)):32, Namespace/binary,
                (byte_size(Name)):32, Name/binary,
                (byte_size(ResourceId)):32, ResourceId/binary>>,
    invoke_host(<<"resource.read">>, Request).

resource_write(Namespace, Name, ValueJson) ->
    resource_write(Namespace, Name, <<"secret/json">>, <<>>, ValueJson).

resource_write(Namespace, Name, Kind, ExpectedResourceId, ValueJson)
        when is_binary(Namespace), is_binary(Name), is_binary(Kind),
             is_binary(ExpectedResourceId), is_binary(ValueJson) ->
    require_resource(<<"resource.write">>, Namespace, Name),
    Request = <<(byte_size(Namespace)):32, Namespace/binary,
                (byte_size(Name)):32, Name/binary,
                (byte_size(Kind)):32, Kind/binary,
                (byte_size(ExpectedResourceId)):32, ExpectedResourceId/binary,
                (byte_size(ValueJson)):32, ValueJson/binary>>,
    invoke_host(<<"resource.write">>, Request).

invoke(Name, Request) when is_binary(Name), is_binary(Request) ->
    require(Name),
    case Name of
        <<"lux.test.echo">> -> Request;
        _ -> invoke_host(Name, Request)
    end.

invoke_host(Name, Request) ->
    case os:getenv("LUX_CAPABILITY_HELPER") of
        false -> erlang:error(capability_helper_not_configured);
        Helper ->
            Port = open_port(
                {spawn_executable, Helper},
                [binary, use_stdio, exit_status, {packet, 4},
                 {args, ["--capability-host", binary_to_list(Name)]}]
            ),
            true = port_command(Port, Request),
            receive
                {Port, {data, Response}} -> Response;
                {Port, {exit_status, Status}} ->
                    erlang:error({capability_helper_exit, Status})
            after 120000 ->
                port_close(Port),
                erlang:error(capability_helper_timeout)
            end
    end.
"#;

#[derive(Debug, Deserialize)]
struct ProviderConfig {
    executable: PathBuf,
    #[serde(default)]
    args: Vec<String>,
}

pub fn prepare_bridge(output_dir: &Path) -> Result<(), ServiceError> {
    fs::create_dir_all(output_dir)?;
    let source_path = output_dir.join("lux_capability.erl");
    let beam_path = output_dir.join("lux_capability.beam");
    if !beam_path.exists() {
        fs::write(&source_path, BRIDGE_SOURCE)?;
        let output = Command::new("erlc")
            .arg("-o")
            .arg(output_dir)
            .arg(&source_path)
            .output()?;
        if !output.status.success() {
            return Err(ServiceError::Io(io::Error::other(format!(
                "capability bridge compilation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))));
        }
    }
    Ok(())
}

pub fn render_grant_setup(capabilities: &[String]) -> String {
    let grants = capabilities
        .iter()
        .map(|capability| format!("<<\"{}\">>", capability))
        .collect::<Vec<_>>()
        .join(", ");
    format!("lux_capability:set_grants([{}]), ", grants)
}

pub fn helper_executable() -> Result<PathBuf, ServiceError> {
    Ok(env::current_exe()?)
}

pub fn run_host(capability: &str) -> io::Result<()> {
    let request = read_packet(io::stdin().lock())?;
    let response = match dispatch(capability, &request) {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    let encoded = serde_json::to_vec(&response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_packet(io::stdout().lock(), &encoded)
}

fn read_packet(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_PACKET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("capability packet exceeds {MAX_PACKET_BYTES} bytes"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn write_packet(mut writer: impl Write, body: &[u8]) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response is too large"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

fn dispatch(capability: &str, request: &[u8]) -> Result<Value, String> {
    if capability == TEST_ECHO_CAPABILITY {
        return serde_json::from_slice(request).map_err(|error| error.to_string());
    }
    if capability == RESOURCE_HEAD_CAPABILITY {
        return head_resource(request);
    }
    if capability == RESOURCE_READ_CAPABILITY {
        return read_resource(request);
    }
    if capability == RESOURCE_WRITE_CAPABILITY {
        return write_resource(request);
    }
    invoke_configured_provider(capability, request)
}

fn head_resource(request: &[u8]) -> Result<Value, String> {
    let database_path = env::var_os("LUX_DATABASE_PATH")
        .ok_or_else(|| "LUX_DATABASE_PATH is not configured".to_string())?;
    head_resource_from(request, Path::new(&database_path))
}

fn head_resource_from(request: &[u8], database_path: &Path) -> Result<Value, String> {
    let (namespace, name, resource_id) = decode_resource_request(request)?;
    if resource_id.is_some() {
        return Err("resource head request must not select a resource id".to_string());
    }
    let store = super::store::SqliteStore::open_read_only(database_path)
        .map_err(|error| format!("could not open resource database: {error}"))?;
    let Some(stored) = store
        .get_namespace_resource(&namespace, &name, None)
        .map_err(|error| format!("could not read resource head: {error}"))?
    else {
        return Ok(json!({
            "found": false,
            "namespace": namespace,
            "name": name,
        }));
    };
    Ok(json!({
        "found": true,
        "resource_id": stored.metadata.resource_id,
        "namespace": stored.metadata.namespace,
        "name": stored.metadata.name,
        "kind": stored.metadata.kind,
        "revision_id": stored.metadata.revision_id,
        "content_hash": stored.metadata.content_hash,
        "created_at_ms": stored.metadata.created_at_ms,
    }))
}

fn read_resource(request: &[u8]) -> Result<Value, String> {
    let database_path = env::var_os("LUX_DATABASE_PATH")
        .ok_or_else(|| "LUX_DATABASE_PATH is not configured".to_string())?;
    let master_key = env::var("LUX_MASTER_KEY_HEX")
        .map_err(|_| "LUX_MASTER_KEY_HEX is not configured".to_string())?;
    let cipher = super::resource::ResourceCipher::from_hex(&master_key)?;
    read_resource_from(request, Path::new(&database_path), &cipher)
}

fn read_resource_from(
    request: &[u8],
    database_path: &Path,
    cipher: &super::resource::ResourceCipher,
) -> Result<Value, String> {
    let (namespace, name, resource_id) = decode_resource_request(request)?;
    let store = super::store::SqliteStore::open_read_only(&database_path)
        .map_err(|error| format!("could not open resource database: {error}"))?;
    let stored = store
        .get_namespace_resource(&namespace, &name, resource_id.as_deref())
        .map_err(|error| format!("could not read resource: {error}"))?
        .ok_or_else(|| format!("resource not found: {namespace}/{name}"))?;
    let plaintext = cipher.decrypt(
        &namespace,
        &name,
        &stored.metadata.kind,
        &stored.nonce,
        &stored.ciphertext,
    )?;
    let value: Value = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("resource value is not valid JSON: {error}"))?;
    Ok(json!({
        "resource_id": stored.metadata.resource_id,
        "namespace": stored.metadata.namespace,
        "name": stored.metadata.name,
        "kind": stored.metadata.kind,
        "revision_id": stored.metadata.revision_id,
        "value": value,
    }))
}

fn write_resource(request: &[u8]) -> Result<Value, String> {
    let database_path = env::var_os("LUX_DATABASE_PATH")
        .ok_or_else(|| "LUX_DATABASE_PATH is not configured".to_string())?;
    let master_key = env::var("LUX_MASTER_KEY_HEX")
        .map_err(|_| "LUX_MASTER_KEY_HEX is not configured".to_string())?;
    let cipher = super::resource::ResourceCipher::from_hex(&master_key)?;
    write_resource_to(request, Path::new(&database_path), &cipher)
}

fn write_resource_to(
    request: &[u8],
    database_path: &Path,
    cipher: &super::resource::ResourceCipher,
) -> Result<Value, String> {
    let (namespace, name, kind, expected_resource_id, value_json) =
        decode_resource_write_request(request)?;
    serde_json::from_slice::<Value>(&value_json)
        .map_err(|error| format!("resource value is not valid JSON: {error}"))?;
    let mut service = super::LiveCodeService::new(
        super::store::SqliteStore::open(database_path)
            .map_err(|error| format!("could not open resource database: {error}"))?,
    );
    let resource = service
        .put_namespace_resource(
            cipher,
            &namespace,
            &name,
            &kind,
            None,
            &value_json,
            expected_resource_id.as_deref(),
        )
        .map_err(|error| format!("resource write failed: {error:?}"))?;
    Ok(json!({
        "resource_id": resource.resource_id,
        "namespace": resource.namespace,
        "name": resource.name,
        "kind": resource.kind,
        "revision_id": resource.revision_id,
        "content_hash": resource.content_hash,
        "created_at_ms": resource.created_at_ms,
    }))
}

fn decode_resource_request(request: &[u8]) -> Result<(String, String, Option<String>), String> {
    let mut remaining = request;
    let namespace = take_resource_field(&mut remaining, "namespace")?;
    let name = take_resource_field(&mut remaining, "name")?;
    let resource_id = take_resource_field(&mut remaining, "resource id")?;
    if !remaining.is_empty() {
        return Err("resource request contains trailing bytes".to_string());
    }
    Ok((
        namespace,
        name,
        (!resource_id.is_empty()).then_some(resource_id),
    ))
}

type ResourceWriteRequest = (String, String, String, Option<String>, Vec<u8>);

fn decode_resource_write_request(request: &[u8]) -> Result<ResourceWriteRequest, String> {
    let mut remaining = request;
    let namespace = take_resource_field(&mut remaining, "namespace")?;
    let name = take_resource_field(&mut remaining, "name")?;
    let kind = take_resource_field(&mut remaining, "kind")?;
    let expected_resource_id = take_resource_field(&mut remaining, "expected resource id")?;
    let value_json = take_resource_bytes(&mut remaining, "value", MAX_PACKET_BYTES)?;
    if !remaining.is_empty() {
        return Err("resource write request contains trailing bytes".to_string());
    }
    Ok((
        namespace,
        name,
        kind,
        (!expected_resource_id.is_empty()).then_some(expected_resource_id),
        value_json,
    ))
}

fn take_resource_field(input: &mut &[u8], label: &str) -> Result<String, String> {
    let value = take_resource_bytes(input, label, 512)?;
    String::from_utf8(value).map_err(|_| format!("resource request {label} is not UTF-8"))
}

fn take_resource_bytes(input: &mut &[u8], label: &str, maximum: usize) -> Result<Vec<u8>, String> {
    if input.len() < 4 {
        return Err(format!("resource request is missing {label} length"));
    }
    let length = u32::from_be_bytes(input[..4].try_into().expect("four-byte length")) as usize;
    *input = &input[4..];
    if length > maximum || input.len() < length {
        return Err(format!("resource request has invalid {label} length"));
    }
    let value = input[..length].to_vec();
    *input = &input[length..];
    Ok(value)
}

fn invoke_configured_provider(capability: &str, request: &[u8]) -> Result<Value, String> {
    let encoded = env::var("LUX_CAPABILITY_PROVIDERS")
        .map_err(|_| "LUX_CAPABILITY_PROVIDERS is not configured".to_string())?;
    let providers: HashMap<String, ProviderConfig> = serde_json::from_str(&encoded)
        .map_err(|error| format!("invalid LUX_CAPABILITY_PROVIDERS configuration: {error}"))?;
    let provider = providers
        .get(capability)
        .ok_or_else(|| format!("no provider is configured for capability {capability}"))?;
    if !provider.executable.is_file() {
        return Err(format!(
            "capability provider does not exist: {}",
            provider.executable.display()
        ));
    }

    let mut child = Command::new(&provider.executable)
        .args(&provider.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "capability provider stdin is unavailable".to_string())?;
    write_packet(&mut stdin, request).map_err(|error| error.to_string())?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "capability provider exited with {}: {}",
            output.status,
            stderr.chars().take(2000).collect::<String>()
        ));
    }
    let response = read_packet(Cursor::new(&output.stdout)).map_err(|error| error.to_string())?;
    serde_json::from_slice(&response)
        .map_err(|error| format!("capability provider returned invalid JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{
        TEST_ECHO_CAPABILITY, dispatch, head_resource_from, read_resource_from, write_resource_to,
    };
    use crate::service::{LiveCodeService, resource::ResourceCipher, store::SqliteStore};

    #[test]
    fn echo_host_round_trips_json() {
        let value = dispatch(TEST_ECHO_CAPABILITY, br#"{"value":42}"#).unwrap();
        assert_eq!(value["value"], 42);
    }

    #[test]
    fn resource_host_reads_and_decrypts_a_namespace_resource() {
        let temp = TempDir::new().unwrap();
        let database_path = temp.path().join("livecode.sqlite3");
        let cipher = ResourceCipher::new([11; 32]);
        let mut service = LiveCodeService::new(SqliteStore::open(&database_path).unwrap());
        service
            .put_namespace_resource(
                &cipher,
                "demo",
                "credentials",
                "secret/json",
                None,
                br#"{"password":"one"}"#,
                None,
            )
            .unwrap();
        drop(service);

        let request = encode_resource_request("demo", "credentials", "");
        let response = read_resource_from(&request, &database_path, &cipher).unwrap();
        assert_eq!(response["namespace"], "demo");
        assert_eq!(response["name"], "credentials");
        assert_eq!(response["value"]["password"], "one");
    }

    #[test]
    fn resource_head_is_optional_metadata_without_plaintext() {
        let temp = TempDir::new().unwrap();
        let database_path = temp.path().join("livecode.sqlite3");
        let cipher = ResourceCipher::new([12; 32]);
        let mut service = LiveCodeService::new(SqliteStore::open(&database_path).unwrap());

        let missing_request = encode_resource_request("demo", "credentials", "");
        let missing = head_resource_from(&missing_request, &database_path).unwrap();
        assert_eq!(missing["found"], false);
        assert!(missing.get("value").is_none());

        let created = service
            .put_namespace_resource(
                &cipher,
                "demo",
                "credentials",
                "secret/json",
                None,
                br#"{"password":"one"}"#,
                None,
            )
            .unwrap();
        drop(service);

        let found = head_resource_from(&missing_request, &database_path).unwrap();
        assert_eq!(found["found"], true);
        assert_eq!(found["resource_id"], created.resource_id);
        assert_eq!(found["content_hash"], created.content_hash);
        assert!(found.get("value").is_none());

        let previous_resource_id = found["resource_id"].as_str().unwrap();
        let update_request = encode_resource_write_request(
            "demo",
            "credentials",
            "secret/json",
            previous_resource_id,
            br#"{"password":"two"}"#,
        );
        let updated = write_resource_to(&update_request, &database_path, &cipher).unwrap();
        assert_ne!(updated["resource_id"], previous_resource_id);

        let latest = head_resource_from(&missing_request, &database_path).unwrap();
        assert_eq!(latest["resource_id"], updated["resource_id"]);
        assert!(latest.get("value").is_none());

        let stale = write_resource_to(&update_request, &database_path, &cipher).unwrap_err();
        assert!(stale.contains("resource head changed"), "{stale}");
    }

    #[test]
    fn resource_host_writes_an_encrypted_namespace_resource() {
        let temp = TempDir::new().unwrap();
        let database_path = temp.path().join("livecode.sqlite3");
        let cipher = ResourceCipher::new([13; 32]);
        drop(SqliteStore::open(&database_path).unwrap());

        let request = encode_resource_write_request(
            "demo",
            "created",
            "secret/json",
            "",
            br#"{"password":"two"}"#,
        );
        let written = write_resource_to(&request, &database_path, &cipher).unwrap();
        assert_eq!(written["namespace"], "demo");
        assert_eq!(written["name"], "created");

        let request = encode_resource_request("demo", "created", "");
        let read = read_resource_from(&request, &database_path, &cipher).unwrap();
        assert_eq!(read["value"]["password"], "two");
    }

    fn encode_resource_request(namespace: &str, name: &str, resource_id: &str) -> Vec<u8> {
        let mut request = Vec::new();
        for field in [namespace, name, resource_id] {
            request.extend_from_slice(&(field.len() as u32).to_be_bytes());
            request.extend_from_slice(field.as_bytes());
        }
        request
    }

    fn encode_resource_write_request(
        namespace: &str,
        name: &str,
        kind: &str,
        expected_resource_id: &str,
        value: &[u8],
    ) -> Vec<u8> {
        let mut request = Vec::new();
        for field in [
            namespace.as_bytes(),
            name.as_bytes(),
            kind.as_bytes(),
            expected_resource_id.as_bytes(),
            value,
        ] {
            request.extend_from_slice(&(field.len() as u32).to_be_bytes());
            request.extend_from_slice(field);
        }
        request
    }
}
