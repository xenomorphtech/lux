use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use tiny_http::{Header, Response, Server, StatusCode};

use super::{
    CompiledPackage, DeploymentRecord, ExecutionLimits, ExecutionRecord, ExecutionResult,
    InstanceRecord, LiveCodeService, NamespaceResource, NamespaceSymbol, NamespaceSymbolKind,
    ServiceError, SourceEdit, run_command_with_limits,
};
use crate::driver::session::{CompileError, SecurityError, SessionConfig};
use crate::syntax::content_address::hash_str;

pub fn serve(addr: &str, service: LiveCodeService, workspace_dir: PathBuf) -> io::Result<()> {
    ApiServer::new(service, workspace_dir).serve(addr)
}

struct ApiServer {
    service: LiveCodeService,
    workspace_dir: PathBuf,
    running_instances: std::collections::HashMap<String, RunningInstance>,
    warm_runtime: Option<WarmRuntime>,
    resource_cipher: Result<Option<super::resource::ResourceCipher>, String>,
    allow_resource_reveal: bool,
}

struct ApiReply {
    status: u16,
    body: String,
}

struct RunningInstance {
    child: Child,
}

struct WarmRuntime {
    child: Child,
    node_name: String,
    cookie: String,
}

#[derive(Deserialize)]
struct PublishRequest {
    namespace: String,
    source: String,
    sandbox: Option<bool>,
    snapshot_id: Option<i64>,
    symbols: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CompileRequest {
    source: String,
    sandbox: Option<bool>,
    snapshot_id: Option<i64>,
}

#[derive(Deserialize)]
struct SnapshotRequest {
    namespace: String,
    generation: Option<i64>,
}

#[derive(Deserialize)]
struct RunRequest {
    snapshot_id: i64,
    target: String,
    #[serde(default)]
    args: Vec<String>,
    limits: Option<ExecutionLimitsRequest>,
}

#[derive(Deserialize)]
struct EvalRequest {
    snapshot_id: i64,
    source: String,
    sandbox: Option<bool>,
    #[serde(default)]
    args: Vec<String>,
    limits: Option<ExecutionLimitsRequest>,
}

#[derive(Deserialize)]
struct PruneExecutionsRequest {
    finished_before_ms: i64,
}

#[derive(Deserialize)]
struct ExecutionLimitsRequest {
    timeout_ms: Option<u64>,
    output_limit_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct DeployRequest {
    snapshot_id: i64,
    target: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize)]
struct SaveChangesetRequest {
    namespace: String,
    changeset: String,
    source: String,
    base_snapshot_id: Option<i64>,
    sandbox: Option<bool>,
    #[serde(default)]
    capabilities: Vec<String>,
    expected_parent_revision_id: Option<String>,
}

#[derive(Deserialize)]
struct ChangesetActionRequest {
    namespace: String,
    changeset: String,
    revision_id: Option<String>,
    symbols: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EditChangesetRequest {
    namespace: String,
    changeset: String,
    revision_id: String,
    edits: Vec<SourceEditRequest>,
}

#[derive(Deserialize)]
struct SourceEditRequest {
    start: usize,
    end: usize,
    replacement: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutSymbolRequest {
    namespace: String,
    kind: String,
    symbol: String,
    source: String,
    expected_generation: i64,
    expected_revision_id: Option<String>,
    sandbox: Option<bool>,
    capabilities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct PutResourceRequest {
    namespace: String,
    name: String,
    #[serde(default = "default_resource_kind")]
    kind: String,
    revision_id: Option<String>,
    expected_resource_id: Option<String>,
    value: Value,
}

#[derive(Deserialize)]
struct ReadResourceRequest {
    namespace: String,
    name: String,
    resource_id: Option<String>,
}

impl ApiServer {
    fn new(mut service: LiveCodeService, workspace_dir: PathBuf) -> Self {
        if let Err(error) = service.index_current_namespace_symbols() {
            eprintln!(
                "Lux could not index existing namespace symbols: {}",
                service_error_message(&error)
            );
        }
        let resource_cipher = match std::env::var("LUX_MASTER_KEY_HEX") {
            Ok(encoded) => super::resource::ResourceCipher::from_hex(&encoded).map(Some),
            Err(std::env::VarError::NotPresent) => {
                let key_path = std::env::var_os("LUX_MASTER_KEY_FILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| workspace_dir.join("resource-key.hex"));
                super::resource::ResourceCipher::load_or_create(&key_path).map(Some)
            }
            Err(error) => Err(error.to_string()),
        };
        let allow_resource_reveal = std::env::var("LUX_ALLOW_RESOURCE_REVEAL")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        Self {
            service,
            workspace_dir,
            running_instances: std::collections::HashMap::new(),
            warm_runtime: None,
            resource_cipher,
            allow_resource_reveal,
        }
    }

    #[cfg(test)]
    fn configure_test_resource_cipher(&mut self, cipher: super::resource::ResourceCipher) {
        self.resource_cipher = Ok(Some(cipher));
        self.allow_resource_reveal = true;
    }

    fn serve(mut self, addr: &str) -> io::Result<()> {
        let server = Server::http(addr).map_err(|err| io::Error::other(err.to_string()))?;
        eprintln!("Listening on http://{}", addr);

        for mut request in server.incoming_requests() {
            let method = request.method().as_str().to_string();
            let url = request.url().to_string();
            let mut body = String::new();
            let reply = match request.as_reader().read_to_string(&mut body) {
                Ok(_) => self.handle_json(&method, &url, &body),
                Err(err) => self.error_reply(400, format!("invalid request body: {}", err)),
            };
            if let Err(err) = request.respond(json_response(reply.status, reply.body)) {
                eprintln!("Failed to respond to request: {}", err);
            }
        }

        Ok(())
    }

    fn compile_package(
        &self,
        source: &str,
        sandbox: bool,
        snapshot_id: Option<i64>,
    ) -> Result<CompiledPackage, ServiceError> {
        let config = compile_config(sandbox);
        match snapshot_id {
            Some(snapshot_id) => {
                self.service
                    .compile_source_in_snapshot(source, snapshot_id, config)
            }
            None => self
                .service
                .compile_source(source, config, &lux_frontend::collections::HashMap::new()),
        }
    }

    fn handle_json(&mut self, method: &str, url: &str, body: &str) -> ApiReply {
        self.reconcile_instances();
        let (path, query) = split_url(url);
        match (method, path) {
            ("GET", "/health") => self.ok_reply(json!({ "status": "ok" })),
            ("GET", "/executions") => self.handle_list_executions(query),
            ("GET", "/instances") => self.handle_list_instances(),
            ("GET", "/namespaces") => self.handle_list_namespaces(),
            ("GET", "/changesets") => self.handle_list_changesets(query),
            ("GET", "/symbols") => self.handle_list_symbols(query),
            ("GET", "/symbol") => self.handle_get_symbol(query),
            ("GET", "/resources") => self.handle_list_resources(query),
            ("POST", "/executions/prune") => self.handle_prune_executions(body),
            ("POST", "/changesets/save") => self.handle_save_changeset(body),
            ("POST", "/changesets/edit") => self.handle_edit_changeset(body),
            ("POST", "/changesets/compile") => self.handle_compile_changeset(body),
            ("POST", "/changesets/publish") => self.handle_publish_changeset(body),
            ("POST", "/symbols/put") => self.handle_put_symbol(body),
            ("POST", "/resources/put") => self.handle_put_resource(body),
            ("POST", "/resources/read") => self.handle_read_resource(body),
            ("POST", "/compile") => self.handle_compile(body),
            ("POST", "/deploy") => self.handle_deploy(body),
            ("POST", "/publish") => self.handle_publish(body),
            ("POST", "/snapshot") => self.handle_snapshot(body),
            ("POST", "/run") => self.handle_run(body),
            ("POST", "/eval") => self.handle_eval(body),
            _ if method == "POST" && path.starts_with("/instances/") && path.ends_with("/stop") => {
                let instance_id = &path["/instances/".len()..path.len() - "/stop".len()];
                self.handle_stop_instance(instance_id)
            }
            _ if method == "GET" && path.starts_with("/instances/") => {
                self.handle_get_instance(&path["/instances/".len()..])
            }
            _ if method == "GET" && path.starts_with("/namespaces/") && path.ends_with("/diff") => {
                let namespace = &path["/namespaces/".len()..path.len() - "/diff".len()];
                self.handle_namespace_diff(namespace, query)
            }
            _ if method == "GET" && path.starts_with("/namespaces/") => {
                self.handle_get_namespace(&path["/namespaces/".len()..], query)
            }
            _ if method == "GET" && path.starts_with("/snapshots/") => {
                self.handle_get_snapshot(&path["/snapshots/".len()..])
            }
            _ if method == "GET" && path.starts_with("/changesets/") => {
                self.handle_get_changeset(&path["/changesets/".len()..], query)
            }
            _ if method == "GET" && path.starts_with("/resources/") => {
                self.handle_get_resource(&path["/resources/".len()..], query)
            }
            _ => self.error_reply(404, format!("unknown route: {} {}", method, path)),
        }
    }

    fn handle_save_changeset(&mut self, body: &str) -> ApiReply {
        let request: SaveChangesetRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        match self.service.save_changeset_revision(
            &request.namespace,
            &request.changeset,
            &request.source,
            request.base_snapshot_id,
            request.sandbox.unwrap_or(false),
            &request.capabilities,
            request.expected_parent_revision_id.as_deref(),
        ) {
            Ok(revision) => self.ok_reply(json!({
                "revision": changeset_revision_json(&revision),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_list_resources(&self, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let Some(namespace) = params.get("namespace") else {
            return self.error_reply(400, "missing namespace query parameter".to_string());
        };
        match self.service.list_namespace_resources(namespace) {
            Ok(resources) => self.ok_reply(json!({
                "resources": resources
                    .iter()
                    .map(namespace_resource_json)
                    .collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_resource(&self, selector: &str, query: Option<&str>) -> ApiReply {
        let Some((namespace, name)) = selector.split_once('/') else {
            return self.error_reply(400, "resource selector must be namespace/name".to_string());
        };
        let params = parse_query_params(query);
        match self.service.get_namespace_resource(
            namespace,
            name,
            params.get("resource_id").map(String::as_str),
        ) {
            Ok(Some(resource)) => self.ok_reply(json!({
                "resource": namespace_resource_json(&resource),
            })),
            Ok(None) => self.error_reply(404, format!("resource not found: {selector}")),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_put_resource(&mut self, body: &str) -> ApiReply {
        let request: PutResourceRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let cipher = match self.configured_resource_cipher() {
            Ok(cipher) => cipher,
            Err(reply) => return reply,
        };
        let plaintext = match serde_json::to_vec(&request.value) {
            Ok(plaintext) => plaintext,
            Err(error) => return self.error_reply(400, error.to_string()),
        };
        match self.service.put_namespace_resource(
            &cipher,
            &request.namespace,
            &request.name,
            &request.kind,
            request.revision_id.as_deref(),
            &plaintext,
            request.expected_resource_id.as_deref(),
        ) {
            Ok(resource) => self.ok_reply(json!({
                "resource": namespace_resource_json(&resource),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_read_resource(&self, body: &str) -> ApiReply {
        if !self.allow_resource_reveal {
            return self.error_reply(
                403,
                "resource reveal is disabled; set LUX_ALLOW_RESOURCE_REVEAL=1 for local administration"
                    .to_string(),
            );
        }
        let request: ReadResourceRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let cipher = match self.configured_resource_cipher() {
            Ok(cipher) => cipher,
            Err(reply) => return reply,
        };
        match self.service.read_namespace_resource(
            &cipher,
            &request.namespace,
            &request.name,
            request.resource_id.as_deref(),
        ) {
            Ok(Some((resource, plaintext))) => {
                let value: Value = match serde_json::from_slice(&plaintext) {
                    Ok(value) => value,
                    Err(error) => {
                        return self.service_error_reply(ServiceError::InvalidRequest(format!(
                            "stored resource is not valid JSON: {error}"
                        )));
                    }
                };
                self.ok_reply(json!({
                    "resource": namespace_resource_json(&resource),
                    "value": value,
                }))
            }
            Ok(None) => self.error_reply(
                404,
                format!("resource not found: {}/{}", request.namespace, request.name),
            ),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn configured_resource_cipher(&self) -> Result<super::resource::ResourceCipher, ApiReply> {
        match &self.resource_cipher {
            Ok(Some(cipher)) => Ok(cipher.clone()),
            Ok(None) => Err(self.error_reply(
                503,
                "protected resources require a configured resource key".to_string(),
            )),
            Err(error) => Err(self.error_reply(500, error.clone())),
        }
    }

    fn handle_edit_changeset(&mut self, body: &str) -> ApiReply {
        let request: EditChangesetRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let edits = request
            .edits
            .into_iter()
            .map(|edit| SourceEdit {
                start: edit.start,
                end: edit.end,
                replacement: edit.replacement,
            })
            .collect::<Vec<_>>();
        match self.service.edit_changeset_revision(
            &request.namespace,
            &request.changeset,
            &request.revision_id,
            &edits,
        ) {
            Ok(revision) => self.ok_reply(json!({
                "revision": changeset_revision_json(&revision),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_list_changesets(&mut self, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let namespace = params.get("namespace").map(String::as_str);
        match self.service.list_changesets(namespace) {
            Ok(changesets) => self.ok_reply(json!({
                "changesets": changesets
                    .into_iter()
                    .map(changeset_summary_json)
                    .collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_list_symbols(&mut self, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let Some(namespace) = params.get("namespace") else {
            return self.error_reply(400, "missing namespace query parameter".to_string());
        };
        let generation = match optional_i64_query(&params, "generation") {
            Ok(generation) => generation,
            Err(message) => return self.error_reply(400, message),
        };
        match self.service.list_namespace_symbols(namespace, generation) {
            Ok((generation, symbols)) => self.ok_reply(json!({
                "namespace": namespace,
                "generation": generation,
                "symbols": symbols
                    .iter()
                    .map(namespace_symbol_metadata_json)
                    .collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_symbol(&mut self, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        if params.contains_key("arity") {
            return self.error_reply(
                400,
                "symbol arity is derived from its definition and is not a selector".to_string(),
            );
        }
        let Some(namespace) = params.get("namespace") else {
            return self.error_reply(400, "missing namespace query parameter".to_string());
        };
        let Some(kind) = params.get("kind") else {
            return self.error_reply(400, "missing kind query parameter".to_string());
        };
        let kind = match NamespaceSymbolKind::parse(kind) {
            Ok(kind) => kind,
            Err(err) => return self.service_error_reply(err),
        };
        let Some(symbol) = params.get("symbol") else {
            return self.error_reply(400, "missing symbol query parameter".to_string());
        };
        let generation = match optional_i64_query(&params, "generation") {
            Ok(generation) => generation,
            Err(message) => return self.error_reply(400, message),
        };
        match self
            .service
            .get_namespace_symbol(namespace, kind, symbol, generation)
        {
            Ok(Some(symbol)) => self.ok_reply(json!({
                "symbol": namespace_symbol_json(&symbol),
            })),
            Ok(None) => self.error_reply(
                404,
                format!("namespace symbol not found: {namespace}/{symbol}"),
            ),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_put_symbol(&mut self, body: &str) -> ApiReply {
        let request: PutSymbolRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let kind = match NamespaceSymbolKind::parse(&request.kind) {
            Ok(kind) => kind,
            Err(err) => return self.service_error_reply(err),
        };
        match self.service.put_namespace_symbol(
            &request.namespace,
            kind,
            &request.symbol,
            &request.source,
            request.expected_generation,
            request.expected_revision_id.as_deref(),
            request.sandbox,
            request.capabilities.as_deref(),
        ) {
            Ok(update) => self.ok_reply(json!({
                "symbol": namespace_symbol_json(&update.symbol),
                "generation": update.generation,
                "snapshot_id": update.snapshot_id,
                "artifacts": update.artifacts.iter().map(|artifact| json!({
                    "symbol": artifact.source_name,
                    "artifact_hash": artifact.artifact_hash,
                    "body_hash": artifact.body_hash,
                    "abi_hash": artifact.abi_hash,
                    "dependencies": artifact.dependencies,
                })).collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_changeset(&mut self, selector: &str, query: Option<&str>) -> ApiReply {
        let Some((namespace, changeset)) = selector.split_once('/') else {
            return self.error_reply(400, "changeset selector must be namespace/name".to_string());
        };
        let params = parse_query_params(query);
        let revision_id = params.get("revision_id").map(String::as_str);
        let summary = match self.service.get_changeset(namespace, changeset) {
            Ok(Some(summary)) => summary,
            Ok(None) => {
                return self.error_reply(
                    404,
                    format!("changeset not found: {}/{}", namespace, changeset),
                );
            }
            Err(err) => return self.service_error_reply(err),
        };
        match self
            .service
            .get_changeset_revision(namespace, changeset, revision_id)
        {
            Ok(Some(revision)) => self.ok_reply(json!({
                "changeset": changeset_summary_json(summary),
                "revision": changeset_revision_json(&revision),
            })),
            Ok(None) => self.error_reply(
                404,
                format!("changeset revision not found: {}/{}", namespace, changeset),
            ),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_compile_changeset(&mut self, body: &str) -> ApiReply {
        let request: ChangesetActionRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        match self.service.compile_changeset(
            &request.namespace,
            &request.changeset,
            request.revision_id.as_deref(),
        ) {
            Ok((revision, build, package)) => self.ok_reply(json!({
                "revision": changeset_revision_metadata_json(&revision),
                "build": changeset_build_json(&build),
                "entry": package_entry_json(&package),
                "artifacts": package_artifacts_json(&package),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_publish_changeset(&mut self, body: &str) -> ApiReply {
        let request: ChangesetActionRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let (revision, build, package) = match self.service.compile_changeset(
            &request.namespace,
            &request.changeset,
            request.revision_id.as_deref(),
        ) {
            Ok(result) => result,
            Err(err) => return self.service_error_reply(err),
        };
        let bindings = match publish_bindings_from_request(&package, request.symbols.as_ref()) {
            Ok(bindings) => bindings,
            Err(message) => return self.error_reply(400, message),
        };
        let generation = match self.service.publish_changeset_bindings(
            &revision.revision_id,
            &request.namespace,
            &bindings,
        ) {
            Ok(generation) => generation,
            Err(err) => return self.service_error_reply(err),
        };
        let snapshot_id = match self
            .service
            .create_snapshot(&request.namespace, Some(generation))
        {
            Ok(snapshot_id) => snapshot_id,
            Err(err) => return self.service_error_reply(err),
        };

        self.ok_reply(json!({
            "namespace": request.namespace,
            "changeset": request.changeset,
            "revision": changeset_revision_metadata_json(&revision),
            "build": changeset_build_json(&build),
            "generation": generation,
            "snapshot_id": snapshot_id,
            "entry": package_entry_json(&package),
            "published_symbols": bindings.iter().map(namespace_binding_json).collect::<Vec<_>>(),
            "artifacts": package_artifacts_json(&package),
        }))
    }

    fn handle_publish(&mut self, body: &str) -> ApiReply {
        let request: PublishRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let package = match self.compile_package(
            &request.source,
            request.sandbox.unwrap_or(false),
            request.snapshot_id,
        ) {
            Ok(package) => package,
            Err(err) => return self.service_error_reply(err),
        };
        if let Err(err) = self.service.store_package(&package) {
            return self.service_error_reply(err);
        }
        let bindings = match publish_bindings_from_request(&package, request.symbols.as_ref()) {
            Ok(bindings) => bindings,
            Err(message) => return self.error_reply(400, message),
        };
        let generation = match self.service.publish_bindings(&request.namespace, &bindings) {
            Ok(generation) => generation,
            Err(err) => return self.service_error_reply(err),
        };

        self.ok_reply(json!({
            "namespace": request.namespace,
            "generation": generation,
            "entry": package_entry_json(&package),
            "published_symbols": bindings.iter().map(namespace_binding_json).collect::<Vec<_>>(),
            "artifacts": package_artifacts_json(&package),
        }))
    }

    fn handle_compile(&mut self, body: &str) -> ApiReply {
        let request: CompileRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        match self.compile_package(
            &request.source,
            request.sandbox.unwrap_or(false),
            request.snapshot_id,
        ) {
            Ok(package) => self.ok_reply(json!({
                "source_module": package.source_module,
                "entry": package_entry_json(&package),
                "artifacts": package_artifacts_json(&package),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_deploy(&mut self, body: &str) -> ApiReply {
        let request: DeployRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let target = match self
            .service
            .resolve_execution_target(request.snapshot_id, &request.target)
        {
            Ok(Some(target)) => target,
            Ok(None) => {
                return self.error_reply(404, format!("unknown target: {}", request.target));
            }
            Err(err) => return self.service_error_reply(err),
        };

        let deployment_id = hash_str(&format!(
            "deployment:{}:{}:{}:{}",
            request.snapshot_id,
            request.target,
            serde_json::to_string(&request.args).unwrap_or_default(),
            now_ms()
        ));
        let instance_id = hash_str(&format!("instance:{}:{}", deployment_id, now_ms()));
        let args_json = serde_json::to_string(&request.args).unwrap_or_else(|_| "[]".to_string());
        let started_at_ms = now_ms();
        let run_dir = self.workspace_dir.join("instances").join(&instance_id);
        if let Err(err) = std::fs::create_dir_all(&run_dir) {
            return self.service_error_reply(ServiceError::Io(err));
        }
        if let Err(err) = self.service.prepare_target_runtime(&target, &run_dir) {
            return self.service_error_reply(err);
        }
        if !target.capabilities.is_empty()
            && let Err(err) = super::capability::prepare_bridge(&run_dir)
        {
            return self.service_error_reply(err);
        }

        let stdout_log = run_dir.join("stdout.log");
        let stderr_log = run_dir.join("stderr.log");
        let child = match spawn_instance_process(
            &target.artifact_hash,
            &request.args,
            &run_dir,
            &stdout_log,
            &stderr_log,
            &target.capabilities,
            self.service.store().path(),
            self.resource_cipher.as_ref().ok().and_then(Option::as_ref),
        ) {
            Ok(child) => child,
            Err(err) => return self.service_error_reply(ServiceError::Io(err)),
        };
        let pid = child.id();

        let deployment = DeploymentRecord {
            deployment_id: deployment_id.clone(),
            snapshot_id: request.snapshot_id,
            entry_artifact_hash: target.artifact_hash.clone(),
            target_selector: request.target.clone(),
            args_json: args_json.clone(),
            created_at_ms: started_at_ms,
        };
        if let Err(err) = self.service.insert_deployment(&deployment) {
            return self.service_error_reply(err);
        }

        let instance = InstanceRecord {
            instance_id: instance_id.clone(),
            deployment_id: deployment_id.clone(),
            snapshot_id: request.snapshot_id,
            entry_artifact_hash: target.artifact_hash.clone(),
            target_selector: request.target,
            args_json,
            status: "running".to_string(),
            started_at_ms,
            stopped_at_ms: None,
            pid: Some(pid),
            exit_code: None,
            run_dir: run_dir.display().to_string(),
        };
        if let Err(err) = self.service.insert_instance(&instance) {
            let _ = kill_child_process(child);
            return self.service_error_reply(err);
        }
        self.running_instances
            .insert(instance_id.clone(), RunningInstance { child });

        self.ok_reply(json!({
            "deployment": deployment_json(&deployment),
            "instance": instance_json(instance),
            "logs": {
                "stdout": stdout_log.display().to_string(),
                "stderr": stderr_log.display().to_string(),
            }
        }))
    }

    fn handle_list_executions(&mut self, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let limit = params
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .map(|value| value.clamp(1, 100))
            .unwrap_or(20);
        let before_started_at_ms = params
            .get("before_started_at_ms")
            .and_then(|value| value.parse::<i64>().ok());
        let snapshot_id = params
            .get("snapshot_id")
            .and_then(|value| value.parse::<i64>().ok());
        let request_kind = params.get("request_kind").map(|value| value.as_str());
        let status = params.get("status").map(|value| value.as_str());

        match self.service.store().list_executions(
            limit,
            before_started_at_ms,
            snapshot_id,
            request_kind,
            status,
        ) {
            Ok(executions) => self.ok_reply(json!({
                "filters": {
                    "limit": limit,
                    "before_started_at_ms": before_started_at_ms,
                    "snapshot_id": snapshot_id,
                    "request_kind": request_kind,
                    "status": status,
                },
                "next_before_started_at_ms": executions.last().map(|execution| execution.started_at_ms),
                "executions": executions.into_iter().map(execution_json).collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(ServiceError::Store(err)),
        }
    }

    fn handle_list_instances(&mut self) -> ApiReply {
        match self.service.list_instances() {
            Ok(instances) => self.ok_reply(json!({
                "instances": instances.into_iter().map(instance_json).collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_instance(&mut self, instance_id: &str) -> ApiReply {
        match self.service.get_instance(instance_id) {
            Ok(Some(instance)) => self.ok_reply(json!({
                "instance": instance_json(instance),
            })),
            Ok(None) => self.error_reply(404, format!("unknown instance: {}", instance_id)),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_stop_instance(&mut self, instance_id: &str) -> ApiReply {
        let Some(mut running) = self.running_instances.remove(instance_id) else {
            return match self.service.get_instance(instance_id) {
                Ok(Some(instance)) => self.ok_reply(json!({
                    "instance": instance_json(instance),
                })),
                Ok(None) => self.error_reply(404, format!("unknown instance: {}", instance_id)),
                Err(err) => self.service_error_reply(err),
            };
        };

        let stopped_at_ms = now_ms();
        let exit_code = match running.child.try_wait() {
            Ok(Some(status)) => status.code(),
            Ok(None) => {
                let _ = running.child.kill();
                let _ = running.child.wait();
                Some(-9)
            }
            Err(_) => Some(-1),
        };
        if let Err(err) = self.service.update_instance_state(
            instance_id,
            "stopped",
            Some(stopped_at_ms),
            exit_code,
        ) {
            return self.service_error_reply(err);
        }
        match self.service.get_instance(instance_id) {
            Ok(Some(instance)) => self.ok_reply(json!({
                "instance": instance_json(instance),
            })),
            Ok(None) => self.error_reply(404, format!("unknown instance: {}", instance_id)),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_list_namespaces(&mut self) -> ApiReply {
        match self.service.list_namespaces() {
            Ok(namespaces) => self.ok_reply(json!({
                "namespaces": namespaces.into_iter().map(namespace_summary_json).collect::<Vec<_>>(),
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_namespace(&mut self, namespace: &str, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let generation = params
            .get("generation")
            .and_then(|value| value.parse::<i64>().ok());
        match self.service.get_namespace_bindings(namespace, generation) {
            Ok(Some((generation, bindings))) => self.ok_reply(json!({
                "namespace": namespace,
                "generation": generation,
                "bindings": bindings.into_iter().map(binding_json).collect::<Vec<_>>(),
            })),
            Ok(None) => self.error_reply(404, format!("unknown namespace: {}", namespace)),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_namespace_diff(&mut self, namespace: &str, query: Option<&str>) -> ApiReply {
        let params = parse_query_params(query);
        let Some(from_generation) = params
            .get("from")
            .and_then(|value| value.parse::<i64>().ok())
        else {
            return self.error_reply(400, "missing from generation".to_string());
        };
        let Some(to_generation) = params.get("to").and_then(|value| value.parse::<i64>().ok())
        else {
            return self.error_reply(400, "missing to generation".to_string());
        };

        match self
            .service
            .diff_namespace_generations(namespace, from_generation, to_generation)
        {
            Ok(Some(diff)) => self.ok_reply(json!({
                "namespace": diff.namespace,
                "from_generation": diff.from_generation,
                "to_generation": diff.to_generation,
                "added": diff.added.into_iter().map(binding_change_json).collect::<Vec<_>>(),
                "removed": diff.removed.into_iter().map(binding_change_json).collect::<Vec<_>>(),
                "changed": diff.changed.into_iter().map(binding_change_json).collect::<Vec<_>>(),
            })),
            Ok(None) => self.error_reply(
                404,
                format!(
                    "unknown namespace or generation range: {} {}..{}",
                    namespace, from_generation, to_generation
                ),
            ),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_get_snapshot(&mut self, snapshot_id: &str) -> ApiReply {
        let snapshot_id = match snapshot_id.parse::<i64>() {
            Ok(snapshot_id) => snapshot_id,
            Err(_) => {
                return self.error_reply(400, format!("invalid snapshot id: {}", snapshot_id));
            }
        };
        match self.service.get_snapshot_summary(snapshot_id) {
            Ok(Some(snapshot)) => self.ok_reply(json!({
                "snapshot_id": snapshot.snapshot_id,
                "namespace": snapshot.namespace,
                "generation": snapshot.generation,
                "bindings": snapshot.bindings.into_iter().map(binding_json).collect::<Vec<_>>(),
            })),
            Ok(None) => self.error_reply(404, format!("unknown snapshot: {}", snapshot_id)),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_prune_executions(&mut self, body: &str) -> ApiReply {
        let request: PruneExecutionsRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        match self
            .service
            .store()
            .prune_executions(request.finished_before_ms)
        {
            Ok(deleted) => self.ok_reply(json!({
                "deleted": deleted,
                "finished_before_ms": request.finished_before_ms,
            })),
            Err(err) => self.service_error_reply(ServiceError::Store(err)),
        }
    }

    fn handle_snapshot(&mut self, body: &str) -> ApiReply {
        let request: SnapshotRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        match self
            .service
            .create_snapshot(&request.namespace, request.generation)
        {
            Ok(snapshot_id) => self.ok_reply(json!({
                "namespace": request.namespace,
                "snapshot_id": snapshot_id,
                "generation": request.generation,
            })),
            Err(err) => self.service_error_reply(err),
        }
    }

    fn handle_run(&mut self, body: &str) -> ApiReply {
        let request: RunRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let started_at_ms = now_ms();
        let start = Instant::now();
        let limits = normalize_execution_limits(request.limits.as_ref());
        let target = match self
            .service
            .resolve_execution_target(request.snapshot_id, &request.target)
        {
            Ok(Some(target)) => target,
            Ok(None) => {
                let reply = self.error_reply(404, format!("unknown target: {}", request.target));
                self.record_execution_summary(build_execution_record(
                    "run",
                    request.snapshot_id,
                    None,
                    Some(request.target),
                    None,
                    "not_found",
                    Some("unknown target".to_string()),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                ));
                return reply;
            }
            Err(err) => {
                let status = service_error_status_label(&err);
                let message = service_error_message(&err);
                let reply = self.service_error_reply(err);
                self.record_execution_summary(build_execution_record(
                    "run",
                    request.snapshot_id,
                    None,
                    Some(request.target),
                    None,
                    status,
                    Some(message),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                ));
                return reply;
            }
        };

        match self.execute_cached_target(&target, &request.args, limits) {
            Ok(result) => {
                let record = build_execution_record(
                    "run",
                    request.snapshot_id,
                    Some(target.artifact_hash.clone()),
                    Some(request.target),
                    None,
                    "success",
                    None,
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    &result.stdout,
                    &result.stderr,
                    &request.args,
                );
                let execution_id = record.execution_id.clone();
                self.record_execution_summary(record);
                self.ok_reply(json!({
                    "execution_id": execution_id,
                    "snapshot_id": request.snapshot_id,
                    "limits": {
                        "timeout_ms": limits.timeout_ms,
                        "output_limit_bytes": limits.output_limit_bytes,
                    },
                    "target": {
                        "artifact_hash": target.artifact_hash,
                    },
                    "stdout": result.stdout,
                    "beam_time_us": result.beam_time_us,
                }))
            }
            Err(err) => {
                let status = service_error_status_label(&err);
                let message = service_error_message(&err);
                let record = build_execution_record(
                    "run",
                    request.snapshot_id,
                    Some(target.artifact_hash),
                    Some(request.target),
                    None,
                    status,
                    Some(message),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                );
                self.record_execution_summary(record);
                self.service_error_reply(err)
            }
        }
    }

    fn handle_eval(&mut self, body: &str) -> ApiReply {
        let request: EvalRequest = match parse_json(body) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let started_at_ms = now_ms();
        let start = Instant::now();
        let config = compile_config(request.sandbox.unwrap_or(false));
        let limits = normalize_execution_limits(request.limits.as_ref());
        let source_hash = hash_str(&request.source);
        let package = match self.service.compile_source_in_snapshot(
            &request.source,
            request.snapshot_id,
            config,
        ) {
            Ok(package) => package,
            Err(err) => {
                let status = service_error_status_label(&err);
                let message = service_error_message(&err);
                let record = build_execution_record(
                    "eval",
                    request.snapshot_id,
                    None,
                    Some("main".to_string()),
                    Some(source_hash),
                    status,
                    Some(message),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                );
                self.record_execution_summary(record);
                return self.service_error_reply(err);
            }
        };
        if let Err(err) = self.service.store_package(&package) {
            let status = service_error_status_label(&err);
            let message = service_error_message(&err);
            let record = build_execution_record(
                "eval",
                request.snapshot_id,
                None,
                Some("main".to_string()),
                Some(source_hash),
                status,
                Some(message),
                started_at_ms,
                start.elapsed().as_millis() as i64,
                "",
                "",
                &request.args,
            );
            self.record_execution_summary(record);
            return self.service_error_reply(err);
        }
        let target = match self.service.entry_target(&package) {
            Ok(target) => target,
            Err(err) => {
                let status = service_error_status_label(&err);
                let message = service_error_message(&err);
                let record = build_execution_record(
                    "eval",
                    request.snapshot_id,
                    None,
                    Some("main".to_string()),
                    Some(source_hash),
                    status,
                    Some(message),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                );
                self.record_execution_summary(record);
                return self.service_error_reply(err);
            }
        };
        match self.execute_cached_target(&target, &request.args, limits) {
            Ok(result) => {
                let record = build_execution_record(
                    "eval",
                    request.snapshot_id,
                    Some(target.artifact_hash.clone()),
                    Some("main".to_string()),
                    Some(source_hash),
                    "success",
                    None,
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    &result.stdout,
                    &result.stderr,
                    &request.args,
                );
                let execution_id = record.execution_id.clone();
                self.record_execution_summary(record);
                self.ok_reply(json!({
                    "execution_id": execution_id,
                    "snapshot_id": request.snapshot_id,
                    "limits": {
                        "timeout_ms": limits.timeout_ms,
                        "output_limit_bytes": limits.output_limit_bytes,
                    },
                    "entry": {
                        "artifact_hash": target.artifact_hash,
                    },
                    "stdout": result.stdout,
                    "beam_time_us": result.beam_time_us,
                    "artifacts": package_artifacts_json(&package),
                }))
            }
            Err(err) => {
                let status = service_error_status_label(&err);
                let message = service_error_message(&err);
                let record = build_execution_record(
                    "eval",
                    request.snapshot_id,
                    Some(target.artifact_hash),
                    Some("main".to_string()),
                    Some(source_hash),
                    status,
                    Some(message),
                    started_at_ms,
                    start.elapsed().as_millis() as i64,
                    "",
                    "",
                    &request.args,
                );
                self.record_execution_summary(record);
                self.service_error_reply(err)
            }
        }
    }

    fn service_error_reply(&self, err: ServiceError) -> ApiReply {
        let status = match &err {
            ServiceError::Compile(_)
            | ServiceError::NoFunctions
            | ServiceError::MissingEntryPoint => 400,
            ServiceError::Timeout { .. } => 408,
            ServiceError::OutputLimitExceeded { .. } => 413,
            ServiceError::Conflict(_) => 409,
            ServiceError::NotFound(_) => 404,
            ServiceError::InvalidRequest(_) => 400,
            ServiceError::Store(_) => 500,
            ServiceError::Io(io_err) => match io_err.kind() {
                io::ErrorKind::NotFound => 404,
                io::ErrorKind::InvalidInput => 400,
                _ => 500,
            },
        };
        self.error_reply(status, service_error_message(&err))
    }

    fn ok_reply(&self, value: Value) -> ApiReply {
        ApiReply {
            status: 200,
            body: value.to_string(),
        }
    }

    fn error_reply(&self, status: u16, message: String) -> ApiReply {
        ApiReply {
            status,
            body: json!({ "error": message }).to_string(),
        }
    }

    fn shared_artifact_cache_dir(&self) -> PathBuf {
        self.workspace_dir.join("artifact-cache")
    }

    fn execute_cached_target(
        &mut self,
        target: &super::ExecutionTarget,
        args: &[String],
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult, ServiceError> {
        if args.len() != target.arity {
            return Err(ServiceError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "entry expects {} args but received {}",
                    target.arity,
                    args.len()
                ),
            )));
        }

        if !command_is_available("erl_call") {
            let run_dir = self
                .workspace_dir
                .join("runs")
                .join(format!("cold-{}", target.artifact_hash));
            return self.service.execute_target_with_limits_and_resource_cipher(
                target,
                args,
                &run_dir,
                limits,
                self.resource_cipher.as_ref().ok().and_then(Option::as_ref),
            );
        }

        let cache_dir = self.shared_artifact_cache_dir();
        self.service
            .prepare_shared_artifact_cache(&target.artifact_hash, &cache_dir)?;
        if !target.capabilities.is_empty() {
            super::capability::prepare_bridge(&cache_dir)?;
        }
        let runtime = self.ensure_warm_runtime(&cache_dir)?;

        let eval = format!(
            "{}\n",
            render_warm_runtime_invocation(
                &target.artifact_hash,
                args,
                limits.timeout_ms,
                &target.capabilities,
            )
        );
        let mut command = Command::new("erl_call");
        command
            .arg("-sname")
            .arg(&runtime.node_name)
            .arg("-c")
            .arg(&runtime.cookie)
            .arg("-fetch_stdout")
            .arg("-no_result_term")
            .arg("-e");

        let client_limits = ExecutionLimits {
            timeout_ms: limits.timeout_ms.saturating_add(1_000),
            output_limit_bytes: limits.output_limit_bytes,
        };
        let output = run_command_with_limits(command, client_limits, Some(eval.into_bytes()))?;
        if !output.status.success() {
            return Err(ServiceError::Io(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            )));
        }
        parse_warm_runtime_output(&output.stdout, &output.stderr, limits.timeout_ms)
    }

    fn ensure_warm_runtime(&mut self, cache_dir: &Path) -> Result<&WarmRuntime, ServiceError> {
        let needs_restart = match self.warm_runtime.as_mut() {
            Some(runtime) => match runtime.child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(_) => true,
            },
            None => true,
        };

        if needs_restart {
            self.warm_runtime = None;
            self.warm_runtime = Some(spawn_warm_runtime(
                cache_dir,
                self.service.store().path(),
                self.resource_cipher.as_ref().ok().and_then(Option::as_ref),
            )?);
        }

        Ok(self
            .warm_runtime
            .as_ref()
            .expect("warm runtime initialized"))
    }

    fn record_execution_summary(&mut self, record: ExecutionRecord) {
        if let Err(err) = self.service.record_execution(&record) {
            eprintln!(
                "Failed to record execution {}: {}",
                record.execution_id,
                service_error_message(&err)
            );
        }
    }

    fn reconcile_instances(&mut self) {
        let mut exited = Vec::new();
        for (instance_id, running) in &mut self.running_instances {
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    exited.push((instance_id.clone(), status.code()));
                }
                Ok(None) => {}
                Err(_) => {
                    exited.push((instance_id.clone(), Some(-1)));
                }
            }
        }
        for (instance_id, exit_code) in exited {
            self.running_instances.remove(&instance_id);
            let _ = self.service.update_instance_state(
                &instance_id,
                "exited",
                Some(now_ms()),
                exit_code,
            );
        }
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        for (_, running) in self.running_instances.drain() {
            let _ = kill_child_process(running.child);
        }
        if let Some(runtime) = self.warm_runtime.take() {
            let _ = kill_child_process(runtime.child);
        }
    }
}

fn spawn_warm_runtime(
    cache_dir: &Path,
    database_path: Option<&Path>,
    resource_cipher: Option<&super::resource::ResourceCipher>,
) -> Result<WarmRuntime, ServiceError> {
    std::fs::create_dir_all(cache_dir)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let node_name = format!("lux_runtime_{}", nonce);
    let cookie = format!("lux_cookie_{}", nonce);
    let mut command = Command::new("erl");
    command
        .arg("-noshell")
        .arg("-sname")
        .arg(&node_name)
        .arg("-setcookie")
        .arg(&cookie)
        .arg("-pa")
        .arg(cache_dir)
        .arg("-eval")
        .arg("receive after infinity -> ok end.")
        .env(
            "LUX_CAPABILITY_HELPER",
            super::capability::helper_executable()?,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = database_path {
        command.env("LUX_DATABASE_PATH", path);
    }
    if let Some(cipher) = resource_cipher {
        command.env("LUX_MASTER_KEY_HEX", cipher.key_hex());
    }
    let child = command.spawn()?;
    let runtime = WarmRuntime {
        child,
        node_name,
        cookie,
    };
    wait_for_warm_runtime(&runtime)?;
    Ok(runtime)
}

fn render_warm_runtime_invocation(
    artifact_hash: &str,
    args: &[String],
    timeout_ms: u64,
    capabilities: &[String],
) -> String {
    let apply = if args.is_empty() {
        format!("'{}':apply()", artifact_hash)
    } else {
        format!("'{}':apply({})", artifact_hash, args.join(", "))
    };

    let capability_setup = if capabilities.is_empty() {
        String::new()
    } else {
        super::capability::render_grant_setup(capabilities)
    };
    format!(
        "Parent = self(), Worker = spawn(fun() -> {}Parent ! {{lux_result, (catch timer:tc(fun() -> {} end))}} end), receive {{lux_result, {{'EXIT', Reason}}}} -> io:format(\"__LUX_ERROR__:~tp~n\", [Reason]); {{lux_result, {{Micros, Value}}}} -> io:format(\"~tp~n__LUX_TC_US__:~B~n\", [Value, Micros]) after {} -> exit(Worker, kill), io:format(\"__LUX_TIMEOUT__~n\", []) end.",
        capability_setup, apply, timeout_ms
    )
}

fn parse_warm_runtime_output(
    stdout: &[u8],
    stderr: &[u8],
    timeout_ms: u64,
) -> Result<ExecutionResult, ServiceError> {
    let rendered = String::from_utf8_lossy(stdout).trim().to_string();
    if rendered.ends_with("__LUX_TIMEOUT__") {
        return Err(ServiceError::Timeout { timeout_ms });
    }
    if let Some(reason) = rendered
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("__LUX_ERROR__:"))
    {
        return Err(ServiceError::Io(std::io::Error::other(reason.to_string())));
    }

    let (stdout, beam_time_us) = super::parse_execution_stdout(stdout);
    Ok(ExecutionResult {
        stdout,
        stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        beam_time_us,
    })
}

fn command_is_available(name: &str) -> bool {
    Command::new(name)
        .arg("-help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn wait_for_warm_runtime(runtime: &WarmRuntime) -> Result<(), ServiceError> {
    let start = Instant::now();
    loop {
        let mut command = Command::new("erl_call");
        command
            .arg("-sname")
            .arg(&runtime.node_name)
            .arg("-c")
            .arg(&runtime.cookie)
            .arg("-a")
            .arg("erlang node []");
        match command.output() {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(_) | Err(_) if start.elapsed().as_millis() < 2_000 => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(output) => {
                return Err(ServiceError::Io(std::io::Error::other(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                )));
            }
            Err(err) => return Err(ServiceError::Io(err)),
        }
    }
}

fn compile_config(sandbox: bool) -> SessionConfig {
    if sandbox {
        SessionConfig::sandboxed_default()
    } else {
        SessionConfig::trusted()
    }
}

fn normalize_execution_limits(request: Option<&ExecutionLimitsRequest>) -> ExecutionLimits {
    let defaults = ExecutionLimits::default();
    let timeout_ms = request
        .and_then(|request| request.timeout_ms)
        .unwrap_or(defaults.timeout_ms)
        .clamp(1, 60_000);
    let output_limit_bytes = request
        .and_then(|request| request.output_limit_bytes)
        .unwrap_or(defaults.output_limit_bytes)
        .clamp(1_024, 1_048_576);
    ExecutionLimits {
        timeout_ms,
        output_limit_bytes,
    }
}

fn package_entry_json(package: &CompiledPackage) -> Value {
    match &package.entry_module {
        Some(module) => json!({
            "artifact_hash": module,
            "function": "apply",
        }),
        None => Value::Null,
    }
}

fn package_artifacts_json(package: &CompiledPackage) -> Value {
    Value::Array(
        package
            .artifacts
            .iter()
            .map(|artifact| {
                json!({
                    "source_name": artifact.source_name,
                    "body_hash": artifact.body_hash,
                    "abi_hash": artifact.abi_hash,
                    "artifact_hash": artifact.artifact_hash,
                    "build_key": artifact.build_key,
                    "dependencies": artifact.dependencies,
                })
            })
            .collect(),
    )
}

fn deployment_json(deployment: &DeploymentRecord) -> Value {
    json!({
        "deployment_id": deployment.deployment_id,
        "snapshot_id": deployment.snapshot_id,
        "entry_artifact_hash": deployment.entry_artifact_hash,
        "target_selector": deployment.target_selector,
        "args_json": deployment.args_json,
        "created_at_ms": deployment.created_at_ms,
    })
}

fn instance_json(instance: super::InstanceRecord) -> Value {
    json!({
        "instance_id": instance.instance_id,
        "deployment_id": instance.deployment_id,
        "snapshot_id": instance.snapshot_id,
        "entry_artifact_hash": instance.entry_artifact_hash,
        "target_selector": instance.target_selector,
        "args_json": instance.args_json,
        "status": instance.status,
        "started_at_ms": instance.started_at_ms,
        "stopped_at_ms": instance.stopped_at_ms,
        "pid": instance.pid,
        "exit_code": instance.exit_code,
        "run_dir": instance.run_dir,
    })
}

fn namespace_summary_json(summary: super::NamespaceSummary) -> Value {
    json!({
        "name": summary.name,
        "current_generation": summary.current_generation,
        "snapshot_count": summary.snapshot_count,
        "binding_count": summary.binding_count,
    })
}

fn namespace_symbol_metadata_json(symbol: &NamespaceSymbol) -> Value {
    json!({
        "namespace": symbol.revision.namespace,
        "kind": symbol.revision.kind.as_str(),
        "symbol": symbol.revision.symbol,
        "declaration_kind": symbol.revision.declaration_kind,
        "revision_id": symbol.revision.revision_id,
        "parent_revision_id": symbol.revision.parent_revision_id,
        "source_hash": symbol.revision.source_hash,
        "compile_context_hash": hash_str(&symbol.revision.compile_context),
        "base_generation": symbol.revision.base_generation,
        "published_generation": symbol.published_generation,
        "artifact_hash": symbol.artifact_hash,
        "sandbox": symbol.revision.sandbox,
        "capabilities": symbol.revision.capabilities,
        "provenance_revision_id": symbol.revision.provenance_revision_id,
        "created_at_ms": symbol.revision.created_at_ms,
    })
}

fn namespace_symbol_json(symbol: &NamespaceSymbol) -> Value {
    let mut value = namespace_symbol_metadata_json(symbol);
    value["source"] = Value::String(symbol.revision.source.clone());
    value
}

fn changeset_summary_json(changeset: super::NamespaceChangeset) -> Value {
    json!({
        "changeset_id": changeset.changeset_id,
        "namespace": changeset.namespace,
        "name": changeset.name,
        "head_revision_id": changeset.head_revision_id,
        "revision_count": changeset.revision_count,
        "created_at_ms": changeset.created_at_ms,
        "updated_at_ms": changeset.updated_at_ms,
    })
}

fn changeset_revision_metadata_json(revision: &super::NamespaceRevision) -> Value {
    json!({
        "revision_id": revision.revision_id,
        "namespace": revision.namespace,
        "changeset": revision.changeset,
        "parent_revision_id": revision.parent_revision_id,
        "base_snapshot_id": revision.base_snapshot_id,
        "source_hash": revision.source_hash,
        "sandbox": revision.sandbox,
        "capabilities": revision.capabilities,
        "created_at_ms": revision.created_at_ms,
    })
}

fn changeset_revision_json(revision: &super::NamespaceRevision) -> Value {
    let mut value = changeset_revision_metadata_json(revision);
    value["source"] = json!(revision.source);
    value
}

fn changeset_build_json(build: &super::ChangesetBuild) -> Value {
    json!({
        "build_id": build.build_id,
        "revision_id": build.revision_id,
        "entry_artifact_hash": build.entry_artifact_hash,
        "artifact_count": build.artifact_count,
        "created_at_ms": build.created_at_ms,
    })
}

fn binding_json(binding: super::PublishedBinding) -> Value {
    json!({
        "symbol": binding.symbol,
        "artifact_hash": binding.artifact_hash,
        "generation": binding.generation,
        "revision_id": binding.revision_id,
    })
}

fn namespace_binding_json(binding: &super::NamespaceBinding) -> Value {
    json!({
        "symbol": binding.symbol,
        "artifact_hash": binding.artifact_hash,
    })
}

fn binding_change_json(change: super::BindingChange) -> Value {
    json!({
        "symbol": change.symbol,
        "from_artifact_hash": change.from_artifact_hash,
        "to_artifact_hash": change.to_artifact_hash,
    })
}

fn publish_bindings_from_request(
    package: &CompiledPackage,
    symbols: Option<&Vec<String>>,
) -> Result<Vec<super::NamespaceBinding>, String> {
    match symbols {
        None => Ok(package
            .artifacts
            .iter()
            .map(|artifact| super::NamespaceBinding {
                symbol: artifact.source_name.clone(),
                arity: artifact.arity,
                artifact_hash: artifact.artifact_hash.clone(),
            })
            .collect()),
        Some(symbols) => {
            let mut bindings = Vec::new();
            for symbol in symbols {
                let Some(artifact) = package
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.source_name == *symbol)
                else {
                    return Err(format!("symbol not found in compiled package: {}", symbol));
                };
                bindings.push(super::NamespaceBinding {
                    symbol: artifact.source_name.clone(),
                    arity: artifact.arity,
                    artifact_hash: artifact.artifact_hash.clone(),
                });
            }
            Ok(bindings)
        }
    }
}

fn spawn_instance_process(
    artifact_hash: &str,
    args: &[String],
    run_dir: &std::path::Path,
    stdout_log: &std::path::Path,
    stderr_log: &std::path::Path,
    capabilities: &[String],
    database_path: Option<&Path>,
    resource_cipher: Option<&super::resource::ResourceCipher>,
) -> Result<Child, io::Error> {
    let invocation = render_deployment_invocation(artifact_hash, args, capabilities);
    let stdout = File::create(stdout_log)?;
    let stderr = File::create(stderr_log)?;
    let mut command = Command::new("erl");
    command
        .arg("-noshell")
        .arg("-pa")
        .arg(run_dir)
        .arg("-eval")
        .arg(invocation)
        .env("LUX_CAPABILITY_HELPER", std::env::current_exe()?)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(path) = database_path {
        command.env("LUX_DATABASE_PATH", path);
    }
    if let Some(cipher) = resource_cipher {
        command.env("LUX_MASTER_KEY_HEX", cipher.key_hex());
    }
    command.spawn()
}

fn render_deployment_invocation(
    artifact_hash: &str,
    args: &[String],
    capabilities: &[String],
) -> String {
    let apply = if args.is_empty() {
        format!("'{}':apply()", artifact_hash)
    } else {
        format!("'{}':apply({})", artifact_hash, args.join(", "))
    };
    let capability_setup = if capabilities.is_empty() {
        String::new()
    } else {
        super::capability::render_grant_setup(capabilities)
    };
    format!(
        "{}case catch {} of {{'EXIT', Reason}} -> io:format(standard_error, \"~p~n\", [Reason]), halt(1); _ -> timer:sleep(infinity) end.",
        capability_setup, apply
    )
}

fn kill_child_process(mut child: Child) -> io::Result<()> {
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn execution_json(record: ExecutionRecord) -> Value {
    json!({
        "execution_id": record.execution_id,
        "request_kind": record.request_kind,
        "snapshot_id": record.snapshot_id,
        "entry_artifact_hash": record.entry_artifact_hash,
        "target_selector": record.target_selector,
        "source_hash": record.source_hash,
        "status": record.status,
        "error_message": record.error_message,
        "started_at_ms": record.started_at_ms,
        "finished_at_ms": record.finished_at_ms,
        "wall_time_ms": record.wall_time_ms,
        "stdout_bytes": record.stdout_bytes,
        "stderr_bytes": record.stderr_bytes,
        "stdout_preview": record.stdout_preview,
        "stderr_preview": record.stderr_preview,
        "arg_fingerprint": record.arg_fingerprint,
    })
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, ApiReply> {
    serde_json::from_str(body).map_err(|err| ApiReply {
        status: 400,
        body: json!({ "error": format!("invalid json: {}", err) }).to_string(),
    })
}

fn default_resource_kind() -> String {
    "secret/json".to_string()
}

fn namespace_resource_json(resource: &NamespaceResource) -> Value {
    json!({
        "resource_id": resource.resource_id,
        "namespace": resource.namespace,
        "name": resource.name,
        "kind": resource.kind,
        "revision_id": resource.revision_id,
        "content_hash": resource.content_hash,
        "created_at_ms": resource.created_at_ms,
    })
}

fn split_url(url: &str) -> (&str, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    }
}

fn parse_query_params(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            params.insert(key.to_string(), value.to_string());
        }
    }
    params
}

fn optional_i64_query(
    params: &std::collections::HashMap<String, String>,
    name: &str,
) -> Result<Option<i64>, String> {
    params
        .get(name)
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| format!("invalid {name} query parameter: {value}"))
        })
        .transpose()
}

fn json_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
    Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header)
}

fn build_execution_record(
    request_kind: &str,
    snapshot_id: i64,
    entry_artifact_hash: Option<String>,
    target_selector: Option<String>,
    source_hash: Option<String>,
    status: &str,
    error_message: Option<String>,
    started_at_ms: i64,
    wall_time_ms: i64,
    stdout: &str,
    stderr: &str,
    args: &[String],
) -> ExecutionRecord {
    let finished_at_ms = started_at_ms.saturating_add(wall_time_ms);
    let stdout_preview = preview(stdout, 4096);
    let stderr_preview = preview(stderr, 4096);
    let arg_fingerprint = hash_str(&serde_json::to_string(args).unwrap_or_default());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let identity = format!(
        "{}:{}:{}:{:?}:{:?}:{}:{}:{}:{}",
        request_kind,
        snapshot_id,
        status,
        entry_artifact_hash,
        target_selector,
        source_hash.as_deref().unwrap_or(""),
        started_at_ms,
        wall_time_ms,
        nonce
    );
    ExecutionRecord {
        execution_id: hash_str(&identity),
        request_kind: request_kind.to_string(),
        snapshot_id,
        entry_artifact_hash,
        target_selector,
        source_hash,
        status: status.to_string(),
        error_message,
        started_at_ms,
        finished_at_ms,
        wall_time_ms,
        stdout_bytes: stdout.len() as i64,
        stderr_bytes: stderr.len() as i64,
        stdout_preview,
        stderr_preview,
        arg_fingerprint,
    }
}

fn preview(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn service_error_message(err: &ServiceError) -> String {
    match err {
        ServiceError::Compile(compile_error) => compile_error_message(compile_error),
        ServiceError::Store(store_error) => format!("store error: {}", store_error),
        ServiceError::Timeout { timeout_ms } => {
            format!("execution timed out after {} ms", timeout_ms)
        }
        ServiceError::OutputLimitExceeded { limit_bytes } => {
            format!("execution output exceeded {} bytes", limit_bytes)
        }
        ServiceError::Io(io_error) => format!("io error: {}", io_error),
        ServiceError::NoFunctions => "no functions to compile".to_string(),
        ServiceError::MissingEntryPoint => "compiled package has no main entry point".to_string(),
        ServiceError::Conflict(message)
        | ServiceError::NotFound(message)
        | ServiceError::InvalidRequest(message) => message.clone(),
    }
}

fn service_error_status_label(err: &ServiceError) -> &'static str {
    match err {
        ServiceError::Compile(_) => "compile_error",
        ServiceError::Store(_) => "store_error",
        ServiceError::Timeout { .. } => "timeout",
        ServiceError::OutputLimitExceeded { .. } => "output_limit_exceeded",
        ServiceError::Io(io_error) => match io_error.kind() {
            io::ErrorKind::NotFound => "not_found",
            io::ErrorKind::InvalidInput => "invalid_input",
            _ => "runtime_error",
        },
        ServiceError::NoFunctions => "no_functions",
        ServiceError::MissingEntryPoint => "missing_entrypoint",
        ServiceError::Conflict(_) => "conflict",
        ServiceError::NotFound(_) => "not_found",
        ServiceError::InvalidRequest(_) => "invalid_request",
    }
}

fn compile_error_message(err: &CompileError) -> String {
    match err {
        CompileError::Parse(parse_error) => {
            format!(
                "parse error at {:?}: {}",
                parse_error.span, parse_error.message
            )
        }
        CompileError::Type(type_error) => format!("type error: {}", type_error),
        CompileError::Io(io_error) => format!("io error: {}", io_error),
        CompileError::Security(SecurityError::ExternDisallowed(span)) => format!(
            "security error at {:?}: external declarations are disabled in sandbox mode",
            span
        ),
        CompileError::Security(SecurityError::ExternModuleDisallowed { module, span }) => format!(
            "security error at {:?}: external module '{}' is not granted",
            span, module
        ),
        CompileError::Security(SecurityError::ImportDisallowed { module, span }) => format!(
            "security error at {:?}: import '{}' is not allowed in sandbox mode",
            span, module
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::service::store::SqliteStore;

    #[test]
    fn symbols_are_created_and_replaced_atomically_without_changeset_source_edits() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let create = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "math",
                "kind": "function",
                "symbol": "answer",
                "source": "fn answer() -> Int { 41 }",
                "expected_generation": 0,
            })
            .to_string(),
        );
        assert_eq!(create.status, 200, "{}", create.body);
        let created: Value = serde_json::from_str(&create.body).unwrap();
        assert_eq!(created["generation"], 1);
        assert_eq!(created["symbol"]["symbol"], "answer");
        assert_eq!(created["symbol"]["source"], "fn answer() -> Int { 41 }");
        assert!(created["symbol"].get("arity").is_none());
        let revision_id = created["symbol"]["revision_id"]
            .as_str()
            .unwrap()
            .to_string();

        let replace = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "math",
                "kind": "function",
                "symbol": "answer",
                "source": "fn answer(value: Int) -> Int { value + 1 }",
                "expected_generation": 1,
                "expected_revision_id": revision_id,
            })
            .to_string(),
        );
        assert_eq!(replace.status, 200, "{}", replace.body);
        let replaced: Value = serde_json::from_str(&replace.body).unwrap();
        assert_eq!(replaced["generation"], 2);
        assert_eq!(
            replaced["symbol"]["source"],
            "fn answer(value: Int) -> Int { value + 1 }"
        );
        assert!(replaced["artifacts"][0].get("arity").is_none());

        let get = api.handle_json(
            "GET",
            "/symbol?namespace=math&kind=function&symbol=answer",
            "",
        );
        assert_eq!(get.status, 200, "{}", get.body);
        assert_eq!(
            serde_json::from_str::<Value>(&get.body).unwrap()["symbol"]["source"],
            "fn answer(value: Int) -> Int { value + 1 }"
        );

        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": replaced["snapshot_id"],
                "target": "answer",
                "args": ["41"],
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        assert_eq!(
            serde_json::from_str::<Value>(&run.body).unwrap()["stdout"],
            "42"
        );

        let caller_supplied_arity = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "math",
                "kind": "function",
                "symbol": "other",
                "arity": 0,
                "source": "fn other() -> Int { 0 }",
                "expected_generation": 2,
            })
            .to_string(),
        );
        assert_eq!(caller_supplied_arity.status, 400);
        assert!(caller_supplied_arity.body.contains("unknown field"));
        assert!(caller_supplied_arity.body.contains("arity"));

        let stale = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "math",
                "kind": "function",
                "symbol": "answer",
                "source": "fn answer() -> Int { 43 }",
                "expected_generation": 1,
                "expected_revision_id": revision_id,
            })
            .to_string(),
        );
        assert_eq!(stale.status, 409, "{}", stale.body);
    }

    #[test]
    fn function_symbol_replacement_rebuilds_only_transitive_dependents() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let (leaf, middle, root, unrelated) = {
            let mut put = |symbol: &str, source: &str, generation: i64| {
                let response = api.handle_json(
                    "POST",
                    "/symbols/put",
                    &json!({
                        "namespace": "deps",
                        "kind": "function",
                        "symbol": symbol,
                        "source": source,
                        "expected_generation": generation,
                    })
                    .to_string(),
                );
                assert_eq!(response.status, 200, "{}", response.body);
                serde_json::from_str::<Value>(&response.body).unwrap()
            };
            let leaf = put("leaf", "fn leaf() -> Int { 1 }", 0);
            let middle = put("middle", "fn middle() -> Int { leaf() + 1 }", 1);
            let root = put("root", "fn root() -> Int { middle() + 1 }", 2);
            let unrelated = put("unrelated", "fn unrelated() -> Int { 99 }", 3);
            (leaf, middle, root, unrelated)
        };

        let replacement = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "deps",
                "kind": "function",
                "symbol": "leaf",
                "source": "fn leaf() -> Int { 10 }",
                "expected_generation": 4,
                "expected_revision_id": leaf["symbol"]["revision_id"],
            })
            .to_string(),
        );
        assert_eq!(replacement.status, 200, "{}", replacement.body);
        let replacement: Value = serde_json::from_str(&replacement.body).unwrap();
        assert_eq!(replacement["generation"], 5);
        assert_eq!(replacement["artifacts"].as_array().unwrap().len(), 3);

        let get = |api: &mut ApiServer, symbol: &str| {
            let response = api.handle_json(
                "GET",
                &format!("/symbol?namespace=deps&kind=function&symbol={symbol}"),
                "",
            );
            assert_eq!(response.status, 200, "{}", response.body);
            serde_json::from_str::<Value>(&response.body).unwrap()["symbol"].clone()
        };
        let current_middle = get(&mut api, "middle");
        let current_root = get(&mut api, "root");
        let current_unrelated = get(&mut api, "unrelated");
        assert_eq!(
            current_middle["revision_id"],
            middle["symbol"]["revision_id"]
        );
        assert_eq!(current_root["revision_id"], root["symbol"]["revision_id"]);
        assert_eq!(
            current_unrelated["revision_id"],
            unrelated["symbol"]["revision_id"]
        );
        assert_ne!(
            current_middle["artifact_hash"],
            middle["symbol"]["artifact_hash"]
        );
        assert_ne!(
            current_root["artifact_hash"],
            root["symbol"]["artifact_hash"]
        );
        assert_eq!(
            current_unrelated["artifact_hash"],
            unrelated["symbol"]["artifact_hash"]
        );

        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": replacement["snapshot_id"],
                "target": "root",
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        assert_eq!(
            serde_json::from_str::<Value>(&run.body).unwrap()["stdout"],
            "12"
        );
    }

    #[test]
    fn incompatible_function_replacement_does_not_advance_the_namespace() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let leaf = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "atomic-deps",
                "kind": "function",
                "symbol": "leaf",
                "source": "fn leaf() -> Int { 1 }",
                "expected_generation": 0,
            })
            .to_string(),
        );
        assert_eq!(leaf.status, 200, "{}", leaf.body);
        let leaf: Value = serde_json::from_str(&leaf.body).unwrap();
        let caller = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "atomic-deps",
                "kind": "function",
                "symbol": "caller",
                "source": "fn caller() -> Int { leaf() }",
                "expected_generation": 1,
            })
            .to_string(),
        );
        assert_eq!(caller.status, 200, "{}", caller.body);
        let caller: Value = serde_json::from_str(&caller.body).unwrap();

        let incompatible = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "atomic-deps",
                "kind": "function",
                "symbol": "leaf",
                "source": "fn leaf(value: Int) -> Int { value }",
                "expected_generation": 2,
                "expected_revision_id": leaf["symbol"]["revision_id"],
            })
            .to_string(),
        );
        assert_eq!(incompatible.status, 400, "{}", incompatible.body);

        let namespace = api.handle_json("GET", "/namespaces/atomic-deps", "");
        assert_eq!(namespace.status, 200, "{}", namespace.body);
        assert_eq!(
            serde_json::from_str::<Value>(&namespace.body).unwrap()["generation"],
            2
        );
        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": caller["snapshot_id"],
                "target": "caller",
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        assert_eq!(
            serde_json::from_str::<Value>(&run.body).unwrap()["stdout"],
            "1"
        );
    }

    #[test]
    fn type_symbol_edit_rebuilds_functions_in_one_generation() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let function = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "typed",
                "kind": "function",
                "symbol": "identity",
                "source": "fn identity(value: Int) -> Int { value }",
                "expected_generation": 0,
            })
            .to_string(),
        );
        assert_eq!(function.status, 200, "{}", function.body);

        let type_edit = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "typed",
                "kind": "type",
                "symbol": "Count",
                "source": "type Count = Int",
                "expected_generation": 1,
            })
            .to_string(),
        );
        assert_eq!(type_edit.status, 200, "{}", type_edit.body);
        let body: Value = serde_json::from_str(&type_edit.body).unwrap();
        assert_eq!(body["generation"], 2);
        assert_eq!(body["symbol"]["kind"], "type");
        assert_eq!(body["artifacts"].as_array().unwrap().len(), 1);

        let list = api.handle_json("GET", "/symbols?namespace=typed", "");
        assert_eq!(list.status, 200, "{}", list.body);
        let symbols = serde_json::from_str::<Value>(&list.body).unwrap()["symbols"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(symbols.len(), 2);
        assert!(symbols.iter().any(|symbol| symbol["kind"] == "function"));
        assert!(symbols.iter().any(|symbol| symbol["kind"] == "type"));
    }

    #[test]
    fn type_only_namespace_has_a_real_generation_and_snapshot() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "types",
                "kind": "type",
                "symbol": "Count",
                "source": "type Count = Int",
                "expected_generation": 0,
            })
            .to_string(),
        );
        assert_eq!(response.status, 200, "{}", response.body);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["generation"], 1);
        assert!(body["artifacts"].as_array().unwrap().is_empty());

        let namespace = api.handle_json("GET", "/namespaces/types", "");
        assert_eq!(namespace.status, 200, "{}", namespace.body);
        assert_eq!(
            serde_json::from_str::<Value>(&namespace.body).unwrap()["generation"],
            1
        );
        let snapshot_id = body["snapshot_id"].as_i64().unwrap();
        let snapshot = api.handle_json("GET", &format!("/snapshots/{snapshot_id}"), "");
        assert_eq!(snapshot.status, 200, "{}", snapshot.body);
    }

    #[test]
    fn symbol_replacement_inherits_hidden_compile_context_and_capability_grants() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        let create_source = r#"extern "erlang" {
    fn lux_capability::invoke(String, String) -> String
}
fn main() { lux_capability::invoke("lux.test.echo", "{\"value\":42}") }"#;
        let create = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "cap-symbol",
                "kind": "function",
                "symbol": "main",
                "source": create_source,
                "expected_generation": 0,
                "sandbox": true,
                "capabilities": ["lux.test.echo"],
            })
            .to_string(),
        );
        assert_eq!(create.status, 200, "{}", create.body);
        let created: Value = serde_json::from_str(&create.body).unwrap();
        assert_eq!(
            created["symbol"]["source"],
            r#"fn main() { lux_capability::invoke("lux.test.echo", "{\"value\":42}") }"#
        );

        let replace = api.handle_json(
            "POST",
            "/symbols/put",
            &json!({
                "namespace": "cap-symbol",
                "kind": "function",
                "symbol": "main",
                "source": r#"fn main() { lux_capability::invoke("lux.test.echo", "{\"value\":43}") }"#,
                "expected_generation": 1,
                "expected_revision_id": created["symbol"]["revision_id"],
            })
            .to_string(),
        );
        assert_eq!(replace.status, 200, "{}", replace.body);
        let replaced: Value = serde_json::from_str(&replace.body).unwrap();
        assert_eq!(replaced["symbol"]["capabilities"], json!(["lux.test.echo"]));

        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": replaced["snapshot_id"],
                "target": "main",
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        assert!(
            serde_json::from_str::<Value>(&run.body).unwrap()["stdout"]
                .as_str()
                .unwrap()
                .contains("43")
        );
    }

    #[test]
    fn protected_resource_api_keeps_values_out_of_metadata_and_versions_heads() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        api.configure_test_resource_cipher(super::super::resource::ResourceCipher::new([9; 32]));

        let put = api.handle_json(
            "POST",
            "/resources/put",
            r#"{"namespace":"demo","name":"credentials","value":{"password":"one","user":"guest"}}"#,
        );
        assert_eq!(put.status, 200, "{}", put.body);
        let put_body: Value = serde_json::from_str(&put.body).unwrap();
        let first_id = put_body["resource"]["resource_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!put.body.contains("password"));
        assert!(!put.body.contains("guest"));

        let get = api.handle_json("GET", "/resources/demo/credentials", "");
        assert_eq!(get.status, 200);
        assert!(!get.body.contains("password"));
        assert_eq!(
            serde_json::from_str::<Value>(&get.body).unwrap()["resource"]["resource_id"],
            first_id
        );

        let list = api.handle_json("GET", "/resources?namespace=demo", "");
        assert_eq!(list.status, 200);
        assert!(!list.body.contains("password"));

        let read = api.handle_json(
            "POST",
            "/resources/read",
            r#"{"namespace":"demo","name":"credentials"}"#,
        );
        assert_eq!(read.status, 200);
        let read_body: Value = serde_json::from_str(&read.body).unwrap();
        assert_eq!(read_body["value"]["password"], "one");

        let conflict = api.handle_json(
            "POST",
            "/resources/put",
            r#"{"namespace":"demo","name":"credentials","value":{"password":"two"}}"#,
        );
        assert_eq!(conflict.status, 409);

        let update = api.handle_json(
            "POST",
            "/resources/put",
            &json!({
                "namespace": "demo",
                "name": "credentials",
                "expected_resource_id": first_id,
                "value": {"password": "two"},
            })
            .to_string(),
        );
        assert_eq!(update.status, 200, "{}", update.body);
        let second_id =
            serde_json::from_str::<Value>(&update.body).unwrap()["resource"]["resource_id"]
                .as_str()
                .unwrap()
                .to_string();
        assert_ne!(second_id, first_id);

        let old = api.handle_json(
            "POST",
            "/resources/read",
            &json!({
                "namespace": "demo",
                "name": "credentials",
                "resource_id": first_id,
            })
            .to_string(),
        );
        assert_eq!(old.status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&old.body).unwrap()["value"]["password"],
            "one"
        );
    }

    #[test]
    fn protected_resource_reveal_requires_explicit_administration_flag() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        api.resource_cipher = Ok(Some(super::super::resource::ResourceCipher::new([3; 32])));
        api.allow_resource_reveal = false;

        let response = api.handle_json(
            "POST",
            "/resources/read",
            r#"{"namespace":"demo","name":"credentials"}"#,
        );
        assert_eq!(response.status, 403);
    }

    #[test]
    fn publish_and_snapshot_requests_return_ids() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let publish = api.handle_json(
            "POST",
            "/publish",
            r#"{"namespace":"dev","source":"fn main() { 1 }"}"#,
        );
        assert_eq!(publish.status, 200);
        let publish_body: Value = serde_json::from_str(&publish.body).unwrap();
        assert_eq!(publish_body["generation"], 1);

        let snapshot = api.handle_json("POST", "/snapshot", r#"{"namespace":"dev"}"#);
        assert_eq!(snapshot.status, 200);
        let snapshot_body: Value = serde_json::from_str(&snapshot.body).unwrap();
        assert_eq!(snapshot_body["snapshot_id"], 1);
    }

    #[test]
    fn run_unknown_target_returns_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service
            .publish_bindings(
                "dev",
                &[super::super::NamespaceBinding {
                    symbol: "fib".to_string(),
                    arity: 1,
                    artifact_hash: "artifact_fib_v1".to_string(),
                }],
            )
            .unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json("POST", "/run", r#"{"snapshot_id":1,"target":"main"}"#);
        assert_eq!(response.status, 404);
        let execution = api.service.store().latest_execution().unwrap().unwrap();
        assert_eq!(execution.request_kind, "run");
        assert_eq!(execution.status, "not_found");
        assert_eq!(execution.target_selector.as_deref(), Some("main"));
    }

    #[test]
    fn eval_without_main_returns_bad_request() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn helper() { 1 }"}"#,
        );
        assert_eq!(response.status, 400);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["error"], "compiled package has no main entry point");
        let execution = api.service.store().latest_execution().unwrap().unwrap();
        assert_eq!(execution.request_kind, "eval");
        assert_eq!(execution.status, "missing_entrypoint");
        assert!(execution.source_hash.is_some());
    }

    #[test]
    fn executions_endpoint_returns_recorded_summaries() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn helper() { 1 }"}"#,
        );

        let response = api.handle_json("GET", "/executions", "");
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        let executions = body["executions"].as_array().unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0]["status"], "missing_entrypoint");
    }

    #[test]
    fn executions_endpoint_supports_filters() {
        let temp_dir = TempDir::new().unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        let mut service = LiveCodeService::new(store);
        service
            .record_execution(&ExecutionRecord {
                execution_id: "exec-run".to_string(),
                request_kind: "run".to_string(),
                snapshot_id: 1,
                entry_artifact_hash: Some("hash_main".to_string()),
                target_selector: Some("main".to_string()),
                source_hash: None,
                status: "success".to_string(),
                error_message: None,
                started_at_ms: 10,
                finished_at_ms: 12,
                wall_time_ms: 2,
                stdout_bytes: 2,
                stderr_bytes: 0,
                stdout_preview: "55".to_string(),
                stderr_preview: String::new(),
                arg_fingerprint: "args1".to_string(),
            })
            .unwrap();
        service
            .record_execution(&ExecutionRecord {
                execution_id: "exec-eval".to_string(),
                request_kind: "eval".to_string(),
                snapshot_id: 2,
                entry_artifact_hash: Some("hash_eval".to_string()),
                target_selector: Some("main".to_string()),
                source_hash: Some("source_hash".to_string()),
                status: "missing_entrypoint".to_string(),
                error_message: Some("missing entry".to_string()),
                started_at_ms: 20,
                finished_at_ms: 21,
                wall_time_ms: 1,
                stdout_bytes: 0,
                stderr_bytes: 12,
                stdout_preview: String::new(),
                stderr_preview: "missing entry".to_string(),
                arg_fingerprint: "args2".to_string(),
            })
            .unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "GET",
            "/executions?request_kind=eval&status=missing_entrypoint&limit=1",
            "",
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        let executions = body["executions"].as_array().unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0]["execution_id"], "exec-eval");
        assert_eq!(body["filters"]["request_kind"], "eval");
        assert_eq!(body["filters"]["status"], "missing_entrypoint");
    }

    #[test]
    fn prune_endpoint_deletes_old_execution_summaries() {
        let temp_dir = TempDir::new().unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        let mut service = LiveCodeService::new(store);
        service
            .record_execution(&ExecutionRecord {
                execution_id: "exec-old".to_string(),
                request_kind: "run".to_string(),
                snapshot_id: 1,
                entry_artifact_hash: Some("hash_old".to_string()),
                target_selector: Some("main".to_string()),
                source_hash: None,
                status: "success".to_string(),
                error_message: None,
                started_at_ms: 1,
                finished_at_ms: 2,
                wall_time_ms: 1,
                stdout_bytes: 2,
                stderr_bytes: 0,
                stdout_preview: "55".to_string(),
                stderr_preview: String::new(),
                arg_fingerprint: "args1".to_string(),
            })
            .unwrap();
        service
            .record_execution(&ExecutionRecord {
                execution_id: "exec-new".to_string(),
                request_kind: "run".to_string(),
                snapshot_id: 1,
                entry_artifact_hash: Some("hash_new".to_string()),
                target_selector: Some("main".to_string()),
                source_hash: None,
                status: "success".to_string(),
                error_message: None,
                started_at_ms: 10,
                finished_at_ms: 11,
                wall_time_ms: 1,
                stdout_bytes: 2,
                stderr_bytes: 0,
                stdout_preview: "89".to_string(),
                stderr_preview: String::new(),
                arg_fingerprint: "args2".to_string(),
            })
            .unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json("POST", "/executions/prune", r#"{"finished_before_ms":10}"#);
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["deleted"], 1);
        assert_eq!(api.service.store().execution_count().unwrap(), 1);
    }

    #[test]
    fn execution_limits_are_defaulted_and_clamped() {
        let defaults = normalize_execution_limits(None);
        assert_eq!(defaults.timeout_ms, 5_000);
        assert_eq!(defaults.output_limit_bytes, 65_536);

        let clamped = normalize_execution_limits(Some(&ExecutionLimitsRequest {
            timeout_ms: Some(0),
            output_limit_bytes: Some(10),
        }));
        assert_eq!(clamped.timeout_ms, 1);
        assert_eq!(clamped.output_limit_bytes, 1_024);

        let capped = normalize_execution_limits(Some(&ExecutionLimitsRequest {
            timeout_ms: Some(120_000),
            output_limit_bytes: Some(10_000_000),
        }));
        assert_eq!(capped.timeout_ms, 60_000);
        assert_eq!(capped.output_limit_bytes, 1_048_576);
    }

    #[test]
    fn namespace_and_snapshot_endpoints_return_bindings() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service
            .publish_bindings(
                "dev",
                &[super::super::NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v1".to_string(),
                }],
            )
            .unwrap();
        let snapshot_id = service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let namespaces = api.handle_json("GET", "/namespaces", "");
        assert_eq!(namespaces.status, 200);
        let namespaces_body: Value = serde_json::from_str(&namespaces.body).unwrap();
        assert_eq!(namespaces_body["namespaces"][0]["name"], "dev");

        let namespace = api.handle_json("GET", "/namespaces/dev", "");
        assert_eq!(namespace.status, 200);
        let namespace_body: Value = serde_json::from_str(&namespace.body).unwrap();
        assert_eq!(namespace_body["generation"], 1);
        assert_eq!(
            namespace_body["bindings"][0]["artifact_hash"],
            "hash_main_v1"
        );

        let snapshot = api.handle_json("GET", &format!("/snapshots/{}", snapshot_id), "");
        assert_eq!(snapshot.status, 200);
        let snapshot_body: Value = serde_json::from_str(&snapshot.body).unwrap();
        assert_eq!(snapshot_body["namespace"], "dev");
        assert_eq!(snapshot_body["bindings"][0]["symbol"], "main");
    }

    #[test]
    fn namespace_diff_endpoint_reports_changes() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        service
            .publish_bindings(
                "dev",
                &[super::super::NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v1".to_string(),
                }],
            )
            .unwrap();
        service
            .publish_bindings(
                "dev",
                &[
                    super::super::NamespaceBinding {
                        symbol: "main".to_string(),
                        arity: 0,
                        artifact_hash: "hash_main_v2".to_string(),
                    },
                    super::super::NamespaceBinding {
                        symbol: "helper".to_string(),
                        arity: 0,
                        artifact_hash: "hash_helper_v1".to_string(),
                    },
                ],
            )
            .unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json("GET", "/namespaces/dev/diff?from=1&to=2", "");
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["changed"][0]["symbol"], "main");
        assert_eq!(body["added"][0]["symbol"], "helper");
    }

    #[test]
    fn compile_endpoint_resolves_against_snapshot_without_publishing() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service
            .publish_bindings(
                "dev",
                &[super::super::NamespaceBinding {
                    symbol: "fib".to_string(),
                    arity: 1,
                    artifact_hash: "artifact_fib_v1".to_string(),
                }],
            )
            .unwrap();
        let snapshot_id = service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/compile",
            &format!(
                "{{\"snapshot_id\":{},\"source\":\"fn main() {{ fib(10) }}\"}}",
                snapshot_id
            ),
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["artifacts"][0]["dependencies"][0], "artifact_fib_v1");
        assert!(body["entry"].get("arity").is_none());
        assert!(body["artifacts"][0].get("arity").is_none());
        assert_eq!(api.service.store().execution_count().unwrap(), 0);
    }

    #[test]
    fn publish_endpoint_can_publish_selected_symbols_only() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/publish",
            r#"{"namespace":"dev","source":"fn helper() { 41 } fn main() { helper() + 1 }","symbols":["main"]}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["published_symbols"].as_array().unwrap().len(), 1);
        assert_eq!(body["published_symbols"][0]["symbol"], "main");

        let namespace = api.handle_json("GET", "/namespaces/dev", "");
        let namespace_body: Value = serde_json::from_str(&namespace.body).unwrap();
        let bindings = namespace_body["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0]["symbol"], "main");
    }

    #[test]
    fn changeset_revisions_are_persisted_and_require_the_current_head() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let created = api.handle_json(
            "POST",
            "/changesets/save",
            r#"{"namespace":"protocol","changeset":"packet-builder","source":"fn main() { 1 }","sandbox":true}"#,
        );
        assert_eq!(created.status, 200);
        let created_body: Value = serde_json::from_str(&created.body).unwrap();
        let revision_id = created_body["revision"]["revision_id"]
            .as_str()
            .unwrap()
            .to_string();

        let fetched = api.handle_json("GET", "/changesets/protocol/packet-builder", "");
        assert_eq!(fetched.status, 200);
        let fetched_body: Value = serde_json::from_str(&fetched.body).unwrap();
        assert_eq!(fetched_body["revision"]["source"], "fn main() { 1 }");
        assert_eq!(fetched_body["changeset"]["head_revision_id"], revision_id);

        let stale = api.handle_json(
            "POST",
            "/changesets/save",
            r#"{"namespace":"protocol","changeset":"packet-builder","source":"fn main() { 2 }","sandbox":true}"#,
        );
        assert_eq!(stale.status, 409);

        let update_body = json!({
            "namespace": "protocol",
            "changeset": "packet-builder",
            "source": "fn main() { 2 }",
            "sandbox": true,
            "expected_parent_revision_id": revision_id,
        })
        .to_string();
        let updated = api.handle_json("POST", "/changesets/save", &update_body);
        assert_eq!(updated.status, 200);
        let updated_body: Value = serde_json::from_str(&updated.body).unwrap();
        assert_eq!(
            updated_body["revision"]["parent_revision_id"],
            created_body["revision"]["revision_id"]
        );
    }

    #[test]
    fn changeset_edits_create_immutable_revisions_from_byte_ranges() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            r#"{"namespace":"protocol","changeset":"packet-builder","source":"fn main() { 1 }"}"#,
        );
        let saved_body: Value = serde_json::from_str(&saved.body).unwrap();
        let revision_id = saved_body["revision"]["revision_id"].as_str().unwrap();
        let edit_body = json!({
            "namespace": "protocol",
            "changeset": "packet-builder",
            "revision_id": revision_id,
            "edits": [{ "start": 12, "end": 13, "replacement": "42" }],
        })
        .to_string();

        let edited = api.handle_json("POST", "/changesets/edit", &edit_body);
        assert_eq!(edited.status, 200);
        let edited_body: Value = serde_json::from_str(&edited.body).unwrap();
        assert_eq!(edited_body["revision"]["source"], "fn main() { 42 }");
        assert_eq!(edited_body["revision"]["parent_revision_id"], revision_id);

        let stale = api.handle_json("POST", "/changesets/edit", &edit_body);
        assert_eq!(stale.status, 409);
    }

    #[test]
    fn changeset_publish_builds_publishes_and_snapshots_the_revision() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            r#"{"namespace":"protocol","changeset":"packet-builder","source":"fn helper() { 41 } fn main() { helper() + 1 }"}"#,
        );
        assert_eq!(saved.status, 200);

        let published = api.handle_json(
            "POST",
            "/changesets/publish",
            r#"{"namespace":"protocol","changeset":"packet-builder","symbols":["main"]}"#,
        );
        assert_eq!(published.status, 200);
        let body: Value = serde_json::from_str(&published.body).unwrap();
        assert_eq!(body["generation"], 1);
        assert!(body["snapshot_id"].as_i64().is_some());
        assert_eq!(body["build"]["artifact_count"], 2);
        assert_eq!(body["published_symbols"].as_array().unwrap().len(), 1);
        assert_eq!(body["published_symbols"][0]["symbol"], "main");

        let snapshot_path = format!("/snapshots/{}", body["snapshot_id"].as_i64().unwrap());
        let snapshot = api.handle_json("GET", &snapshot_path, "");
        assert_eq!(snapshot.status, 200);
        let snapshot_body: Value = serde_json::from_str(&snapshot.body).unwrap();
        assert_eq!(snapshot_body["bindings"][0]["symbol"], "main");
    }

    #[test]
    fn capability_grants_follow_revision_into_snapshot_execution() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        let source = r#"
            extern "erlang" {
                fn lux_capability::invoke(String, String) -> String
            }
            fn main() {
                lux_capability::invoke("lux.test.echo", "{\"value\":42}")
            }
        "#;
        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            &json!({
                "namespace": "runtime",
                "changeset": "capability-echo",
                "source": source,
                "sandbox": true,
                "capabilities": ["lux.test.echo"],
            })
            .to_string(),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let saved_body: Value = serde_json::from_str(&saved.body).unwrap();
        assert_eq!(
            saved_body["revision"]["capabilities"],
            json!(["lux.test.echo"])
        );

        let published = api.handle_json(
            "POST",
            "/changesets/publish",
            r#"{"namespace":"runtime","changeset":"capability-echo","symbols":["main"]}"#,
        );
        assert_eq!(published.status, 200, "{}", published.body);
        let published_body: Value = serde_json::from_str(&published.body).unwrap();
        let snapshot_id = published_body["snapshot_id"].as_i64().unwrap();

        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": snapshot_id,
                "target": "main",
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        let run_body: Value = serde_json::from_str(&run.body).unwrap();
        assert!(run_body["stdout"].as_str().unwrap().contains("value"));
    }

    #[test]
    fn udp_capability_keeps_a_socket_handle_across_lux_calls() {
        let echo = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        echo.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let port = echo.local_addr().unwrap().port();
        let echo_thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (length, peer) = echo.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..length], b"ping");
            echo.send_to(b"pong", peer).unwrap();
        });

        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        let source = format!(
            r#"
                extern "erlang" {{
                    fn lux_capability::udp_open() -> Int
                    fn lux_capability::udp_send(Int, String, Int, String) -> Int
                    fn lux_capability::udp_recv(Int, Int) -> String
                    fn lux_capability::udp_recv_optional(Int, Int) -> String
                    fn lux_capability::udp_close(Int) -> Atom
                }}
                fn main() {{
                    let socket = lux_capability::udp_open()
                    let _sent = lux_capability::udp_send(socket, "127.0.0.1", {port}, "ping")
                    let response = lux_capability::udp_recv(socket, 1000)
                    let timed_out = lux_capability::udp_recv_optional(socket, 5)
                    lux_capability::udp_close(socket)
                    response == "pong" && timed_out == ""
                }}
            "#
        );
        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            &json!({
                "namespace": "runtime",
                "changeset": "udp-echo",
                "source": source,
                "sandbox": true,
                "capabilities": ["net.udp"],
            })
            .to_string(),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let published = api.handle_json(
            "POST",
            "/changesets/publish",
            r#"{"namespace":"runtime","changeset":"udp-echo","symbols":["main"]}"#,
        );
        assert_eq!(published.status, 200, "{}", published.body);
        let published_body: Value = serde_json::from_str(&published.body).unwrap();
        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": published_body["snapshot_id"],
                "target": "main",
            })
            .to_string(),
        );
        echo_thread.join().unwrap();
        assert_eq!(run.status, 200, "{}", run.body);
        let run_body: Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run_body["stdout"], "true");
    }

    #[test]
    fn generic_crypto_clock_and_random_capabilities_run_in_lux() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        let source = r#"
            extern "erlang" {
                fn lux_capability::random_bytes(Int) -> String
                fn lux_capability::sha256(String) -> String
                fn lux_capability::mod_pow(String, String, String) -> String
                fn lux_capability::aes_256_cbc(String, String, String, Bool) -> String
                fn lux_capability::wall_time_ms() -> Int
            }
            fn main() {
                let key = <<0:256>>
                let iv = <<0:128>>
                let plaintext = <<0:128>>
                let encrypted = lux_capability::aes_256_cbc(plaintext, key, iv, true)
                let decrypted = lux_capability::aes_256_cbc(encrypted, key, iv, false)
                lux_capability::sha256("abc") == <<186,120,22,191,143,1,207,234,65,65,64,222,93,174,34,35,176,3,97,163,150,23,122,156,180,16,255,97,242,0,21,173>>
                    && lux_capability::mod_pow(<<2>>, <<5>>, <<13>>) == <<6>>
                    && decrypted == plaintext
                    && byte_size(lux_capability::random_bytes(32)) == 32
                    && lux_capability::wall_time_ms() > 0
            }
        "#;
        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            &json!({
                "namespace": "runtime",
                "changeset": "generic-crypto",
                "source": source,
                "sandbox": true,
                "capabilities": [
                    "clock.wall",
                    "crypto.block",
                    "crypto.hash",
                    "crypto.modular",
                    "crypto.random"
                ],
            })
            .to_string(),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let published = api.handle_json(
            "POST",
            "/changesets/publish",
            r#"{"namespace":"runtime","changeset":"generic-crypto","symbols":["main"]}"#,
        );
        assert_eq!(published.status, 200, "{}", published.body);
        let published_body: Value = serde_json::from_str(&published.body).unwrap();
        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": published_body["snapshot_id"],
                "target": "main",
            })
            .to_string(),
        );
        assert_eq!(run.status, 200, "{}", run.body);
        let run_body: Value = serde_json::from_str(&run.body).unwrap();
        assert_eq!(run_body["stdout"], "true");
    }

    #[test]
    fn runtime_rejects_capability_names_not_granted_to_the_revision() {
        let temp_dir = TempDir::new().unwrap();
        let service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());
        let source = r#"
            extern "erlang" {
                fn lux_capability::invoke(String, String) -> String
            }
            fn main() { lux_capability::invoke("lux.test.ungranted", "{}") }
        "#;
        let saved = api.handle_json(
            "POST",
            "/changesets/save",
            &json!({
                "namespace": "runtime",
                "changeset": "capability-denied",
                "source": source,
                "sandbox": true,
                "capabilities": ["lux.test.echo"],
            })
            .to_string(),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let published = api.handle_json(
            "POST",
            "/changesets/publish",
            r#"{"namespace":"runtime","changeset":"capability-denied","symbols":["main"]}"#,
        );
        assert_eq!(published.status, 200, "{}", published.body);
        let published_body: Value = serde_json::from_str(&published.body).unwrap();

        let run = api.handle_json(
            "POST",
            "/run",
            &json!({
                "snapshot_id": published_body["snapshot_id"],
                "target": "main",
            })
            .to_string(),
        );
        assert_eq!(run.status, 500, "{}", run.body);
        assert!(run.body.contains("capability_not_granted"), "{}", run.body);
    }

    #[test]
    fn eval_binary_segment_matches_binary_string() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() { <<0x44434241:32>> == \"ABCD\" }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_binary_slice_handles_non_utf8_protocol_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() { binary_slice(<<0, 255, 1>>, 1, 1) == <<255>> }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_binary_patterns_and_dynamic_predicates_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() { match <<0x44434241:32>> { <<0x41:8, rest>> => is_string(rest) && !is_nil(rest) && byte_size(rest) == 3, _ => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_pipe_to_local_function_works() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn double(x: Int) -> Int { x * 2 } fn main() -> Int { 5 |> double }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "10");
    }

    #[test]
    fn eval_concat_works_for_binary_strings() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { (\"Hello\" ++ \" World\") == \"Hello World\" }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_binary_specifiers_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { match <<65/utf8, \"BC\"/binary>> { <<x/utf8, rest/binary>> => x == 65 && rest == \"BC\", _ => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_big_endian_integer_segments_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { <<0x1234:16/big, 0x01020304:32/big>> == <<0x12,0x34,1,2,3,4>> && match <<0xabcd:16/big>> { <<value:16/big>> => value == 0xabcd, _ => false } }"}"#,
        );
        assert_eq!(response.status, 200, "{}", response.body);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_dynamic_json_decode_and_encode_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { let value: Dynamic = dynamic_json_decode(\"{\\\"name\\\":\\\"Lux\\\",\\\"items\\\":[1,2]}\") let encoded = dynamic_json_encode(dynamic(%{\"status\" => \"ok\", \"mode\" => \"demo\"})) match decode_then(dynamic_get_result(value, \"items\"), |items| decode_then(dynamic_at_result(items, 1), |item| dynamic_int_result(item))) { DynamicResult::Ok(first) => dynamic_is_map(value) && first == 1 && str_contains(encoded, \"\\\"status\\\":\\\"ok\\\"\"), DynamicResult::Err(_) => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_dynamic_shape_combinators_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { let payload = dynamic_json_decode(\"{\\\"name\\\":\\\"Lux\\\",\\\"items\\\":[1,2,3],\\\"title\\\":7}\") match (decode_field(payload, \"name\", |value| dynamic_string_result(value)), decode_field(payload, \"items\", |items| decode_list(items, |value| dynamic_int_result(value))), decode_field(payload, \"title\", |value| decode_one_of(value, |text| dynamic_string_result(text), |number| decode_map(dynamic_int_result(number), |n| str_from_chars(integer_to_list(n)))))) { (DynamicResult::Ok(name), DynamicResult::Ok(items), DynamicResult::Ok(title)) => name == \"Lux\" && nth(3, items) == 3 && title == \"7\", _ => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_dynamic_optional_and_dict_combinators_work() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { let payload = dynamic_json_decode(\"{\\\"nickname\\\":null,\\\"scores\\\":{\\\"a\\\":1,\\\"b\\\":2}}\") match (decode_optional_field(payload, \"nickname\", |item| dynamic_string_result(item)), decode_field(payload, \"scores\", |value| decode_dict(value, |item| dynamic_int_result(item))), decode_field_or(payload, \"mode\", \"demo\", |value| dynamic_string_result(value))) { (DynamicResult::Ok(DynamicOption::None), DynamicResult::Ok(scores), DynamicResult::Ok(mode)) => map_get(scores, \"b\") == 2 && mode == \"demo\", _ => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn eval_dynamic_decode_errors_are_structured() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        let generation = service.publish_bindings("dev", &[]).unwrap();
        service.create_snapshot("dev", Some(generation)).unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let response = api.handle_json(
            "POST",
            "/eval",
            r#"{"snapshot_id":1,"source":"fn main() -> Bool { let payload = dynamic_json_decode(\"{\\\"name\\\":7}\") match decode_field(payload, \"name\", |value| dynamic_string_result(value)) { DynamicResult::Err(DecodeError::At(DecodePathItem::Field(field), DecodeError::Expected(kind))) => field == \"name\" && kind == \"string\", _ => false } }"}"#,
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["stdout"], "true");
    }

    #[test]
    fn instances_endpoints_return_persisted_instances() {
        let temp_dir = TempDir::new().unwrap();
        let mut service = LiveCodeService::new(SqliteStore::open_in_memory().unwrap());
        service
            .insert_deployment(&super::super::DeploymentRecord {
                deployment_id: "dep_1".to_string(),
                snapshot_id: 1,
                entry_artifact_hash: "hash_main".to_string(),
                target_selector: "main".to_string(),
                args_json: "[]".to_string(),
                created_at_ms: 10,
            })
            .unwrap();
        service
            .insert_instance(&super::super::InstanceRecord {
                instance_id: "inst_1".to_string(),
                deployment_id: "dep_1".to_string(),
                snapshot_id: 1,
                entry_artifact_hash: "hash_main".to_string(),
                target_selector: "main".to_string(),
                args_json: "[]".to_string(),
                status: "running".to_string(),
                started_at_ms: 11,
                stopped_at_ms: None,
                pid: Some(1234),
                exit_code: None,
                run_dir: "/tmp/inst_1".to_string(),
            })
            .unwrap();
        let mut api = ApiServer::new(service, temp_dir.path().to_path_buf());

        let list = api.handle_json("GET", "/instances", "");
        assert_eq!(list.status, 200);
        let list_body: Value = serde_json::from_str(&list.body).unwrap();
        assert_eq!(list_body["instances"][0]["instance_id"], "inst_1");

        let get = api.handle_json("GET", "/instances/inst_1", "");
        assert_eq!(get.status, 200);
        let get_body: Value = serde_json::from_str(&get.body).unwrap();
        assert_eq!(get_body["instance"]["status"], "running");
    }
}
