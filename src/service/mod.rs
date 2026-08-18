use lux_frontend::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use crate::codegen::emit::Emitter;
use crate::codegen::erlang::{CoreExpr, CoreModule};
use crate::codegen::translate::Translator;
use crate::driver::session::{CompileError, Session, SessionConfig};
use crate::syntax::ast::Item;
use crate::syntax::content_address::{hash_bytes, hash_str};
use crate::syntax::lexer::Lexer;
use crate::syntax::parser::Parser;

pub mod api;
pub mod capability;
pub mod resource;
pub mod store;

#[derive(Debug, Clone)]
pub struct NamespaceBinding {
    pub symbol: String,
    pub arity: usize,
    pub artifact_hash: String,
}

#[derive(Debug, Clone)]
pub struct NamespaceSummary {
    pub name: String,
    pub current_generation: Option<i64>,
    pub snapshot_count: i64,
    pub binding_count: i64,
}

#[derive(Debug, Clone)]
pub struct PublishedBinding {
    pub symbol: String,
    pub arity: usize,
    pub artifact_hash: String,
    pub generation: i64,
    pub revision_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceSymbolKind {
    Function,
    Type,
}

impl NamespaceSymbolKind {
    pub fn parse(value: &str) -> Result<Self, ServiceError> {
        match value {
            "function" => Ok(Self::Function),
            "type" => Ok(Self::Type),
            _ => Err(ServiceError::InvalidRequest(format!(
                "namespace symbol kind must be `function` or `type`, got {value}"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamespaceSymbolRevision {
    pub revision_id: String,
    pub namespace: String,
    pub kind: NamespaceSymbolKind,
    pub symbol: String,
    pub arity: usize,
    pub declaration_kind: String,
    pub parent_revision_id: Option<String>,
    pub base_generation: i64,
    pub source_hash: String,
    pub source: String,
    pub compile_context: String,
    pub sandbox: bool,
    pub capabilities: Vec<String>,
    pub provenance_revision_id: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct NamespaceSymbol {
    pub revision: NamespaceSymbolRevision,
    pub published_generation: i64,
    pub artifact_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NamespaceBindingUpdate {
    pub binding: NamespaceBinding,
    pub revision_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NamespaceSymbolUpdate {
    pub symbol: NamespaceSymbol,
    pub generation: i64,
    pub snapshot_id: i64,
    pub artifacts: Vec<FunctionArtifact>,
}

#[derive(Debug, Clone)]
pub struct SnapshotSummary {
    pub snapshot_id: i64,
    pub namespace: String,
    pub generation: i64,
    pub bindings: Vec<PublishedBinding>,
}

#[derive(Debug, Clone)]
pub struct BindingChange {
    pub symbol: String,
    pub arity: usize,
    pub from_artifact_hash: Option<String>,
    pub to_artifact_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NamespaceDiff {
    pub namespace: String,
    pub from_generation: i64,
    pub to_generation: i64,
    pub added: Vec<BindingChange>,
    pub removed: Vec<BindingChange>,
    pub changed: Vec<BindingChange>,
}

#[derive(Debug, Clone)]
pub struct NamespaceChangeset {
    pub changeset_id: i64,
    pub namespace: String,
    pub name: String,
    pub head_revision_id: Option<String>,
    pub revision_count: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct NamespaceRevision {
    pub revision_id: String,
    pub namespace: String,
    pub changeset: String,
    pub parent_revision_id: Option<String>,
    pub base_snapshot_id: Option<i64>,
    pub source_hash: String,
    pub source: String,
    pub sandbox: bool,
    pub capabilities: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SourceEdit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct ChangesetBuild {
    pub build_id: String,
    pub revision_id: String,
    pub entry_artifact_hash: Option<String>,
    pub entry_arity: Option<usize>,
    pub artifact_count: i64,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct NamespaceResource {
    pub resource_id: String,
    pub namespace: String,
    pub name: String,
    pub kind: String,
    pub revision_id: Option<String>,
    pub content_hash: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct StoredNamespaceResource {
    pub metadata: NamespaceResource,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DeploymentRecord {
    pub deployment_id: String,
    pub snapshot_id: i64,
    pub entry_artifact_hash: String,
    pub target_selector: String,
    pub args_json: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct InstanceRecord {
    pub instance_id: String,
    pub deployment_id: String,
    pub snapshot_id: i64,
    pub entry_artifact_hash: String,
    pub target_selector: String,
    pub args_json: String,
    pub status: String,
    pub started_at_ms: i64,
    pub stopped_at_ms: Option<i64>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub run_dir: String,
}

#[derive(Debug, Clone)]
pub struct FunctionArtifact {
    pub source_name: String,
    pub body_hash: String,
    pub abi_hash: String,
    pub artifact_hash: String,
    pub build_key: String,
    pub arity: usize,
    pub core_source: String,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CompiledPackage {
    pub source_module: String,
    pub entry_module: Option<String>,
    pub entry_arity: Option<usize>,
    pub artifacts: Vec<FunctionArtifact>,
}

#[derive(Debug, Clone)]
pub struct ExecutionTarget {
    pub artifact_hash: String,
    pub arity: usize,
    pub revision_id: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub beam_time_us: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub timeout_ms: u64,
    pub output_limit_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            output_limit_bytes: 65_536,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvaluatedPackage {
    pub package: CompiledPackage,
    pub target: ExecutionTarget,
    pub result: ExecutionResult,
}

#[derive(Debug, Clone)]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub request_kind: String,
    pub snapshot_id: i64,
    pub entry_artifact_hash: Option<String>,
    pub target_selector: Option<String>,
    pub source_hash: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub wall_time_ms: i64,
    pub stdout_bytes: i64,
    pub stderr_bytes: i64,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub arg_fingerprint: String,
}

#[derive(Debug)]
pub enum ServiceError {
    Compile(CompileError),
    Store(rusqlite::Error),
    Io(std::io::Error),
    NoFunctions,
    MissingEntryPoint,
    Timeout { timeout_ms: u64 },
    OutputLimitExceeded { limit_bytes: usize },
    Conflict(String),
    NotFound(String),
    InvalidRequest(String),
}

impl From<CompileError> for ServiceError {
    fn from(value: CompileError) -> Self {
        Self::Compile(value)
    }
}

impl From<rusqlite::Error> for ServiceError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Store(value)
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub struct LiveCodeService {
    store: store::SqliteStore,
}

impl LiveCodeService {
    pub fn new(store: store::SqliteStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &store::SqliteStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut store::SqliteStore {
        &mut self.store
    }

    pub fn compile_source(
        &self,
        source: &str,
        config: SessionConfig,
        resolved_symbols: &HashMap<(String, usize), String>,
    ) -> Result<CompiledPackage, ServiceError> {
        let mut session = Session::with_config(config);
        let module = session.compile_source(source)?;
        let source_module = module.name.clone().unwrap_or_else(|| "main".to_string());

        let mut translator = Translator::new();
        let translated =
            translator.translate_function_modules_with_resolution(&module, resolved_symbols);
        if translated.modules.is_empty() {
            return Err(ServiceError::NoFunctions);
        }

        let artifacts = translated
            .modules
            .iter()
            .zip(translated.metadata.iter())
            .map(|(core_module, metadata)| {
                let mut emitter = Emitter::new();
                let core_source = emitter.emit_module(core_module);
                let dependencies = collect_apply_dependencies(core_module, &metadata.module_hash);
                FunctionArtifact {
                    source_name: metadata.source_name.clone(),
                    body_hash: metadata.body_hash.clone(),
                    abi_hash: metadata.abi_hash.clone(),
                    artifact_hash: metadata.module_hash.clone(),
                    build_key: metadata.build_key.clone(),
                    arity: metadata.arity,
                    core_source,
                    dependencies,
                }
            })
            .collect();

        Ok(CompiledPackage {
            source_module,
            entry_module: translated.entry_module,
            entry_arity: translated.entry_arity,
            artifacts,
        })
    }

    pub fn compile_source_in_snapshot(
        &self,
        source: &str,
        snapshot_id: i64,
        mut config: SessionConfig,
    ) -> Result<CompiledPackage, ServiceError> {
        let resolved_symbols = self.store.resolve_snapshot_bindings(snapshot_id)?;
        config.resolved_functions = resolved_symbols
            .keys()
            .cloned()
            .map(|key| (key, ()))
            .collect();
        self.compile_source(source, config, &resolved_symbols)
    }

    pub fn store_package(&mut self, package: &CompiledPackage) -> Result<(), ServiceError> {
        self.store.insert_package(package)?;
        Ok(())
    }

    pub fn publish_bindings(
        &mut self,
        namespace: &str,
        bindings: &[NamespaceBinding],
    ) -> Result<i64, ServiceError> {
        Ok(self.store.publish_bindings(namespace, bindings)?)
    }

    pub fn save_changeset_revision(
        &mut self,
        namespace: &str,
        changeset: &str,
        source: &str,
        base_snapshot_id: Option<i64>,
        sandbox: bool,
        capabilities: &[String],
        expected_parent_revision_id: Option<&str>,
    ) -> Result<NamespaceRevision, ServiceError> {
        if namespace.trim().is_empty() {
            return Err(ServiceError::InvalidRequest(
                "namespace must not be empty".to_string(),
            ));
        }
        if changeset.trim().is_empty() {
            return Err(ServiceError::InvalidRequest(
                "changeset name must not be empty".to_string(),
            ));
        }

        let capabilities = canonicalize_capabilities(capabilities)?;
        if !capabilities.is_empty() && !sandbox {
            return Err(ServiceError::InvalidRequest(
                "capability grants require sandbox=true".to_string(),
            ));
        }

        let current = self.store.get_changeset(namespace, changeset)?;
        let actual_parent = current
            .as_ref()
            .and_then(|changeset| changeset.head_revision_id.as_deref());
        if actual_parent != expected_parent_revision_id {
            return Err(ServiceError::Conflict(format!(
                "changeset head changed: expected {}, found {}",
                expected_parent_revision_id.unwrap_or("<none>"),
                actual_parent.unwrap_or("<none>")
            )));
        }

        if let Some(snapshot_id) = base_snapshot_id
            && self.store.get_snapshot_summary(snapshot_id)?.is_none()
        {
            return Err(ServiceError::NotFound(format!(
                "snapshot {} does not exist",
                snapshot_id
            )));
        }

        let source_hash = hash_str(source);
        let capability_identity = capabilities.join(":");
        let identity = format!(
            "namespace-revision-v2:{}:{}:{}:{}:{}:{}:{}",
            namespace,
            changeset,
            expected_parent_revision_id.unwrap_or(""),
            base_snapshot_id
                .map(|value| value.to_string())
                .unwrap_or_default(),
            sandbox,
            capability_identity,
            source_hash
        );
        let revision = NamespaceRevision {
            revision_id: hash_str(&identity),
            namespace: namespace.to_string(),
            changeset: changeset.to_string(),
            parent_revision_id: expected_parent_revision_id.map(str::to_string),
            base_snapshot_id,
            source_hash,
            source: source.to_string(),
            sandbox,
            capabilities,
            created_at_ms: unix_time_ms(),
        };
        self.store.insert_changeset_revision(&revision)?;
        Ok(revision)
    }

    pub fn edit_changeset_revision(
        &mut self,
        namespace: &str,
        changeset: &str,
        revision_id: &str,
        edits: &[SourceEdit],
    ) -> Result<NamespaceRevision, ServiceError> {
        let revision = self
            .store
            .get_changeset_revision(namespace, changeset, Some(revision_id))?
            .ok_or_else(|| {
                ServiceError::NotFound(format!(
                    "changeset revision not found: {}/{}@{}",
                    namespace, changeset, revision_id
                ))
            })?;
        let source = apply_source_edits(&revision.source, edits)?;
        self.save_changeset_revision(
            namespace,
            changeset,
            &source,
            revision.base_snapshot_id,
            revision.sandbox,
            &revision.capabilities,
            Some(&revision.revision_id),
        )
    }

    pub fn list_changesets(
        &self,
        namespace: Option<&str>,
    ) -> Result<Vec<NamespaceChangeset>, ServiceError> {
        Ok(self.store.list_changesets(namespace)?)
    }

    pub fn get_changeset(
        &self,
        namespace: &str,
        changeset: &str,
    ) -> Result<Option<NamespaceChangeset>, ServiceError> {
        Ok(self.store.get_changeset(namespace, changeset)?)
    }

    pub fn get_changeset_revision(
        &self,
        namespace: &str,
        changeset: &str,
        revision_id: Option<&str>,
    ) -> Result<Option<NamespaceRevision>, ServiceError> {
        Ok(self
            .store
            .get_changeset_revision(namespace, changeset, revision_id)?)
    }

    pub fn compile_changeset(
        &mut self,
        namespace: &str,
        changeset: &str,
        revision_id: Option<&str>,
    ) -> Result<(NamespaceRevision, ChangesetBuild, CompiledPackage), ServiceError> {
        let revision = self
            .store
            .get_changeset_revision(namespace, changeset, revision_id)?
            .ok_or_else(|| {
                ServiceError::NotFound(format!(
                    "changeset revision not found: {}/{}",
                    namespace, changeset
                ))
            })?;
        let config = if revision.sandbox && !revision.capabilities.is_empty() {
            SessionConfig::capability_sandboxed()
        } else if revision.sandbox {
            SessionConfig::sandboxed_default()
        } else {
            SessionConfig::trusted()
        };
        let package = match revision.base_snapshot_id {
            Some(snapshot_id) => {
                self.compile_source_in_snapshot(&revision.source, snapshot_id, config)?
            }
            None => self.compile_source(&revision.source, config, &HashMap::new())?,
        };
        self.store.insert_package(&package)?;

        let artifact_identity = package
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_hash.as_str())
            .collect::<Vec<_>>()
            .join(":");
        let build = ChangesetBuild {
            build_id: hash_str(&format!(
                "changeset-build:{}:{}",
                revision.revision_id, artifact_identity
            )),
            revision_id: revision.revision_id.clone(),
            entry_artifact_hash: package.entry_module.clone(),
            entry_arity: package.entry_arity,
            artifact_count: package.artifacts.len() as i64,
            created_at_ms: unix_time_ms(),
        };
        self.store.insert_changeset_build(&build, &package)?;
        Ok((revision, build, package))
    }

    pub fn publish_changeset_bindings(
        &mut self,
        revision_id: &str,
        namespace: &str,
        bindings: &[NamespaceBinding],
    ) -> Result<i64, ServiceError> {
        let generation =
            self.store
                .publish_bindings_with_revision(namespace, bindings, Some(revision_id))?;
        if let Some((_, published)) = self
            .store
            .get_namespace_bindings(namespace, Some(generation))?
        {
            self.ensure_namespace_symbol_index(namespace, generation, &published)?;
        }
        Ok(generation)
    }

    pub fn list_namespace_symbols(
        &self,
        namespace: &str,
        generation: Option<i64>,
    ) -> Result<(i64, Vec<NamespaceSymbol>), ServiceError> {
        let (resolved_generation, _) = self
            .store
            .get_namespace_bindings(namespace, generation)?
            .ok_or_else(|| ServiceError::NotFound(format!("namespace not found: {namespace}")))?;
        Ok((
            resolved_generation,
            self.store
                .list_namespace_symbols(namespace, resolved_generation)?,
        ))
    }

    pub fn get_namespace_symbol(
        &self,
        namespace: &str,
        kind: NamespaceSymbolKind,
        symbol: &str,
        generation: Option<i64>,
    ) -> Result<Option<NamespaceSymbol>, ServiceError> {
        let (_, symbols) = self.list_namespace_symbols(namespace, generation)?;
        Ok(symbols.into_iter().find(|candidate| {
            candidate.revision.kind == kind && candidate.revision.symbol == symbol
        }))
    }

    pub fn index_current_namespace_symbols(&mut self) -> Result<(), ServiceError> {
        for namespace in self.store.list_namespaces()? {
            let Some(generation) = namespace.current_generation else {
                continue;
            };
            let Some((_, bindings)) = self
                .store
                .get_namespace_bindings(&namespace.name, Some(generation))?
            else {
                continue;
            };
            self.ensure_namespace_symbol_index(&namespace.name, generation, &bindings)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_namespace_symbol(
        &mut self,
        namespace: &str,
        kind: NamespaceSymbolKind,
        symbol: &str,
        source: &str,
        expected_generation: i64,
        expected_revision_id: Option<&str>,
        sandbox: Option<bool>,
        capabilities: Option<&[String]>,
    ) -> Result<NamespaceSymbolUpdate, ServiceError> {
        let current_generation = self
            .store
            .current_namespace_generation(namespace)?
            .unwrap_or(0);
        if current_generation != expected_generation {
            return Err(ServiceError::Conflict(format!(
                "namespace generation conflict: expected {expected_generation}, current {current_generation}"
            )));
        }

        let (resolved_generation, bindings) = self
            .store
            .get_namespace_bindings(namespace, Some(current_generation))?
            .unwrap_or((current_generation, Vec::new()));
        self.ensure_namespace_symbol_index(namespace, resolved_generation, &bindings)?;
        let current_symbols = self
            .store
            .list_namespace_symbols(namespace, current_generation)?;
        let current = current_symbols.iter().find(|candidate| {
            candidate.revision.kind == kind && candidate.revision.symbol == symbol
        });
        match (current, expected_revision_id) {
            (Some(current), Some(expected))
                if current.revision.revision_id.as_str() == expected => {}
            (Some(current), Some(expected)) => {
                return Err(ServiceError::Conflict(format!(
                    "symbol revision conflict: expected {expected}, current {}",
                    current.revision.revision_id
                )));
            }
            (Some(current), None) => {
                return Err(ServiceError::Conflict(format!(
                    "symbol already exists at revision {}; expected_revision_id is required to replace it",
                    current.revision.revision_id
                )));
            }
            (None, Some(expected)) => {
                return Err(ServiceError::Conflict(format!(
                    "symbol does not exist; cannot replace expected revision {expected}"
                )));
            }
            (None, None) => {}
        }

        let (parsed, submitted_context) = validate_single_symbol_source(source, &kind, symbol)?;
        let inherited_sandbox = current.map(|value| value.revision.sandbox).unwrap_or(true);
        let inherited_capabilities = current
            .map(|value| value.revision.capabilities.clone())
            .unwrap_or_default();
        let sandbox = sandbox.unwrap_or(inherited_sandbox);
        let capabilities = match capabilities {
            Some(capabilities) => canonicalize_capabilities(capabilities)?,
            None => inherited_capabilities,
        };
        if !sandbox && !capabilities.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "capability grants require sandbox=true".to_string(),
            ));
        }
        let compile_context = if submitted_context.is_empty() {
            current
                .map(|value| value.revision.compile_context.clone())
                .unwrap_or_default()
        } else {
            submitted_context
        };
        let revision = make_namespace_symbol_revision(
            namespace,
            kind.clone(),
            symbol,
            parsed.arity,
            &parsed.declaration_kind,
            &parsed.source,
            &compile_context,
            current.map(|value| value.revision.revision_id.as_str()),
            current_generation,
            sandbox,
            capabilities,
            None,
        );

        let mut binding_updates = Vec::new();
        let artifacts = match kind {
            NamespaceSymbolKind::Function => {
                let mut definitions = current_symbols
                    .iter()
                    .filter(|candidate| candidate.revision.kind == NamespaceSymbolKind::Type)
                    .map(|candidate| candidate.revision.source.clone())
                    .collect::<Vec<_>>();
                if !revision.compile_context.is_empty() {
                    definitions.insert(0, revision.compile_context.clone());
                }
                definitions.push(revision.source.clone());
                let dependency_bindings = bindings
                    .iter()
                    .filter(|binding| binding.symbol != symbol)
                    .cloned()
                    .collect::<Vec<_>>();
                let package = self.compile_symbol_definitions(
                    &definitions,
                    symbol_compile_config(sandbox, &revision.capabilities),
                    &dependency_bindings,
                )?;
                if package.artifacts.len() != 1
                    || package.artifacts[0].source_name != symbol
                    || package.artifacts[0].arity != parsed.arity
                {
                    return Err(ServiceError::InvalidRequest(format!(
                        "function symbol edit for {symbol} produced an unexpected artifact"
                    )));
                }
                self.store.insert_package(&package)?;
                let artifact = package.artifacts[0].clone();
                let edited_binding = NamespaceBindingUpdate {
                    binding: NamespaceBinding {
                        symbol: symbol.to_string(),
                        arity: parsed.arity,
                        artifact_hash: artifact.artifact_hash.clone(),
                    },
                    revision_id: Some(revision.revision_id.clone()),
                };
                binding_updates.push(edited_binding.clone());

                // Artifacts call immutable dependency hashes. Replacing a symbol therefore
                // has to derive fresh artifacts for callers that reference the old hash;
                // otherwise the namespace would advertise the new binding while its callers
                // silently kept executing the previous revision. Only the transitive inbound
                // dependency closure is rebuilt, and those symbols keep their source revision.
                let mut artifacts = package.artifacts;
                let mut resolved_bindings = bindings.clone();
                if let Some(binding) = resolved_bindings
                    .iter_mut()
                    .find(|binding| binding.symbol == symbol)
                {
                    binding.arity = edited_binding.binding.arity;
                    binding.artifact_hash = edited_binding.binding.artifact_hash.clone();
                    binding.revision_id = edited_binding.revision_id.clone();
                } else {
                    resolved_bindings.push(PublishedBinding {
                        symbol: edited_binding.binding.symbol.clone(),
                        arity: edited_binding.binding.arity,
                        artifact_hash: edited_binding.binding.artifact_hash.clone(),
                        generation: current_generation,
                        revision_id: edited_binding.revision_id.clone(),
                    });
                }

                let mut replaced_artifact_hashes = HashSet::new();
                if let Some(old_hash) = current.and_then(|symbol| symbol.artifact_hash.as_deref()) {
                    replaced_artifact_hashes.insert(old_hash.to_string());
                }
                let mut pending_dependents = current_symbols
                    .iter()
                    .filter(|candidate| {
                        candidate.revision.kind == NamespaceSymbolKind::Function
                            && candidate.revision.symbol != symbol
                    })
                    .collect::<Vec<_>>();

                loop {
                    let mut rebuilt_any = false;
                    let mut remaining = Vec::new();
                    for candidate in pending_dependents {
                        let old_hash = candidate.artifact_hash.as_deref().ok_or_else(|| {
                            ServiceError::Conflict(format!(
                                "function symbol {} has no published artifact",
                                candidate.revision.symbol
                            ))
                        })?;
                        let dependencies = self.store.get_artifact_dependencies(old_hash)?;
                        if !dependencies
                            .iter()
                            .any(|dependency| replaced_artifact_hashes.contains(dependency))
                        {
                            remaining.push(candidate);
                            continue;
                        }

                        let mut dependent_definitions = current_symbols
                            .iter()
                            .filter(|symbol| symbol.revision.kind == NamespaceSymbolKind::Type)
                            .map(|symbol| symbol.revision.source.clone())
                            .collect::<Vec<_>>();
                        if !candidate.revision.compile_context.is_empty() {
                            dependent_definitions
                                .insert(0, candidate.revision.compile_context.clone());
                        }
                        dependent_definitions.push(candidate.revision.source.clone());
                        let dependency_bindings = resolved_bindings
                            .iter()
                            .filter(|binding| binding.symbol != candidate.revision.symbol)
                            .cloned()
                            .collect::<Vec<_>>();
                        let dependent_package = self.compile_symbol_definitions(
                            &dependent_definitions,
                            symbol_compile_config(
                                candidate.revision.sandbox,
                                &candidate.revision.capabilities,
                            ),
                            &dependency_bindings,
                        )?;
                        if dependent_package.artifacts.len() != 1
                            || dependent_package.artifacts[0].source_name
                                != candidate.revision.symbol
                            || dependent_package.artifacts[0].arity != candidate.revision.arity
                        {
                            return Err(ServiceError::InvalidRequest(format!(
                                "rebuilding dependent symbol {} produced an unexpected artifact",
                                candidate.revision.symbol
                            )));
                        }
                        self.store.insert_package(&dependent_package)?;
                        let dependent_artifact = dependent_package.artifacts[0].clone();
                        let update = NamespaceBindingUpdate {
                            binding: NamespaceBinding {
                                symbol: candidate.revision.symbol.clone(),
                                arity: candidate.revision.arity,
                                artifact_hash: dependent_artifact.artifact_hash.clone(),
                            },
                            revision_id: Some(candidate.revision.revision_id.clone()),
                        };
                        let resolved = resolved_bindings
                            .iter_mut()
                            .find(|binding| binding.symbol == candidate.revision.symbol)
                            .ok_or_else(|| {
                                ServiceError::Conflict(format!(
                                    "dependent symbol {} has no active binding",
                                    candidate.revision.symbol
                                ))
                            })?;
                        resolved.arity = update.binding.arity;
                        resolved.artifact_hash = update.binding.artifact_hash.clone();
                        resolved.revision_id = update.revision_id.clone();
                        binding_updates.push(update);
                        artifacts.extend(dependent_package.artifacts);
                        replaced_artifact_hashes.insert(old_hash.to_string());
                        rebuilt_any = true;
                    }
                    if !rebuilt_any {
                        break;
                    }
                    pending_dependents = remaining;
                }
                artifacts
            }
            NamespaceSymbolKind::Type => {
                let mut definitions = current_symbols
                    .iter()
                    .filter(|candidate| {
                        candidate.revision.kind == NamespaceSymbolKind::Type
                            && candidate.revision.symbol != symbol
                    })
                    .map(|candidate| candidate.revision.source.clone())
                    .collect::<Vec<_>>();
                definitions.push(revision.source.clone());
                let function_symbols = current_symbols
                    .iter()
                    .filter(|candidate| candidate.revision.kind == NamespaceSymbolKind::Function)
                    .collect::<Vec<_>>();
                let mut contexts = function_symbols
                    .iter()
                    .map(|candidate| candidate.revision.compile_context.clone())
                    .filter(|context| !context.is_empty())
                    .collect::<Vec<_>>();
                contexts.sort();
                contexts.dedup();
                definitions.splice(0..0, contexts);
                definitions.extend(
                    function_symbols
                        .iter()
                        .map(|candidate| candidate.revision.source.clone()),
                );
                if function_symbols.len() != bindings.len() {
                    return Err(ServiceError::Conflict(
                        "cannot atomically edit a type until every published function has indexed symbol source"
                            .to_string(),
                    ));
                }
                if function_symbols.is_empty() {
                    Vec::new()
                } else {
                    let config = namespace_compile_config(&function_symbols);
                    let package = self.compile_symbol_definitions(&definitions, config, &[])?;
                    if package.artifacts.len() != function_symbols.len() {
                        return Err(ServiceError::InvalidRequest(
                            "type edit did not rebuild every published function".to_string(),
                        ));
                    }
                    self.store.insert_package(&package)?;
                    for artifact in &package.artifacts {
                        let owner = function_symbols
                            .iter()
                            .find(|candidate| candidate.revision.symbol == artifact.source_name)
                            .ok_or_else(|| {
                                ServiceError::InvalidRequest(format!(
                                    "rebuilt unexpected function {}",
                                    artifact.source_name
                                ))
                            })?;
                        binding_updates.push(NamespaceBindingUpdate {
                            binding: NamespaceBinding {
                                symbol: artifact.source_name.clone(),
                                arity: artifact.arity,
                                artifact_hash: artifact.artifact_hash.clone(),
                            },
                            revision_id: Some(owner.revision.revision_id.clone()),
                        });
                    }
                    package.artifacts
                }
            }
        };

        self.store.insert_symbol_revision(&revision)?;
        let generation = self.store.publish_symbol_update(
            namespace,
            current_generation,
            &binding_updates,
            &revision,
        )?;
        let snapshot_id = self.create_snapshot(namespace, Some(generation))?;
        let symbol = self
            .store
            .list_namespace_symbols(namespace, generation)?
            .into_iter()
            .find(|candidate| candidate.revision.revision_id == revision.revision_id)
            .ok_or_else(|| {
                ServiceError::NotFound(format!(
                    "published symbol revision not found: {}",
                    revision.revision_id
                ))
            })?;
        Ok(NamespaceSymbolUpdate {
            symbol,
            generation,
            snapshot_id,
            artifacts,
        })
    }

    fn compile_symbol_definitions(
        &self,
        definitions: &[String],
        mut config: SessionConfig,
        bindings: &[PublishedBinding],
    ) -> Result<CompiledPackage, ServiceError> {
        let source = definitions.join("\n\n");
        let resolved = bindings
            .iter()
            .map(|binding| {
                (
                    (binding.symbol.clone(), binding.arity),
                    binding.artifact_hash.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        config.resolved_functions = resolved
            .keys()
            .cloned()
            .map(|selector| (selector, ()))
            .collect();
        self.compile_source(&source, config, &resolved)
    }

    fn ensure_namespace_symbol_index(
        &mut self,
        namespace: &str,
        generation: i64,
        bindings: &[PublishedBinding],
    ) -> Result<(), ServiceError> {
        let mut revisions = HashMap::<String, NamespaceRevision>::new();
        for binding in bindings {
            let Some(revision_id) = binding.revision_id.as_deref() else {
                continue;
            };
            if !revisions.contains_key(revision_id) {
                let Some(revision) = self.store.get_changeset_revision_by_id(revision_id)? else {
                    continue;
                };
                revisions.insert(revision_id.to_string(), revision);
            }
        }

        for (provenance_revision_id, revision) in revisions {
            let ParsedNamespaceSource {
                symbols,
                compile_context,
            } = parse_namespace_source(&revision.source)?;
            for parsed in symbols {
                if parsed.kind == NamespaceSymbolKind::Function
                    && !bindings.iter().any(|binding| {
                        binding.revision_id.as_deref() == Some(provenance_revision_id.as_str())
                            && binding.symbol == parsed.symbol
                    })
                {
                    continue;
                }
                let mut symbol_revision = make_namespace_symbol_revision(
                    namespace,
                    parsed.kind,
                    &parsed.symbol,
                    parsed.arity,
                    &parsed.declaration_kind,
                    &parsed.source,
                    &compile_context,
                    None,
                    generation,
                    revision.sandbox,
                    revision.capabilities.clone(),
                    Some(provenance_revision_id.clone()),
                );
                symbol_revision.created_at_ms = revision.created_at_ms;
                self.store.insert_symbol_revision(&symbol_revision)?;
                self.store
                    .publish_symbol_at_generation(&symbol_revision, generation)?;
            }
        }
        Ok(())
    }

    pub fn put_namespace_resource(
        &mut self,
        cipher: &resource::ResourceCipher,
        namespace: &str,
        name: &str,
        kind: &str,
        revision_id: Option<&str>,
        plaintext: &[u8],
        expected_resource_id: Option<&str>,
    ) -> Result<NamespaceResource, ServiceError> {
        validate_resource_identity(namespace, name, kind)?;
        let current = self.store.get_namespace_resource(namespace, name, None)?;
        let actual_resource_id = current
            .as_ref()
            .map(|resource| resource.metadata.resource_id.as_str());
        if actual_resource_id != expected_resource_id {
            return Err(ServiceError::Conflict(format!(
                "resource head changed: expected {}, found {}",
                expected_resource_id.unwrap_or("<none>"),
                actual_resource_id.unwrap_or("<none>")
            )));
        }

        let content_hash = hash_bytes(plaintext);
        let resource_id = hash_str(&format!(
            "namespace-resource-v1:{namespace}:{name}:{kind}:{}:{content_hash}",
            expected_resource_id.unwrap_or("")
        ));
        let (nonce, ciphertext) = cipher
            .encrypt(namespace, name, kind, plaintext)
            .map_err(ServiceError::InvalidRequest)?;
        let metadata = NamespaceResource {
            resource_id,
            namespace: namespace.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            revision_id: revision_id.map(str::to_string),
            content_hash,
            created_at_ms: unix_time_ms(),
        };
        self.store
            .insert_namespace_resource(&StoredNamespaceResource {
                metadata: metadata.clone(),
                nonce,
                ciphertext,
            })?;
        Ok(metadata)
    }

    pub fn get_namespace_resource(
        &self,
        namespace: &str,
        name: &str,
        resource_id: Option<&str>,
    ) -> Result<Option<NamespaceResource>, ServiceError> {
        Ok(self
            .store
            .get_namespace_resource(namespace, name, resource_id)?
            .map(|resource| resource.metadata))
    }

    pub fn read_namespace_resource(
        &self,
        cipher: &resource::ResourceCipher,
        namespace: &str,
        name: &str,
        resource_id: Option<&str>,
    ) -> Result<Option<(NamespaceResource, Vec<u8>)>, ServiceError> {
        let Some(resource) = self
            .store
            .get_namespace_resource(namespace, name, resource_id)?
        else {
            return Ok(None);
        };
        let plaintext = cipher
            .decrypt(
                namespace,
                name,
                &resource.metadata.kind,
                &resource.nonce,
                &resource.ciphertext,
            )
            .map_err(ServiceError::InvalidRequest)?;
        Ok(Some((resource.metadata, plaintext)))
    }

    pub fn list_namespace_resources(
        &self,
        namespace: &str,
    ) -> Result<Vec<NamespaceResource>, ServiceError> {
        Ok(self.store.list_namespace_resources(namespace)?)
    }

    pub fn create_snapshot(
        &mut self,
        namespace: &str,
        generation: Option<i64>,
    ) -> Result<i64, ServiceError> {
        Ok(self.store.create_snapshot(namespace, generation)?)
    }

    pub fn publish_package(
        &mut self,
        namespace: &str,
        package: &CompiledPackage,
    ) -> Result<i64, ServiceError> {
        let bindings: Vec<NamespaceBinding> = package
            .artifacts
            .iter()
            .map(|artifact| NamespaceBinding {
                symbol: artifact.source_name.clone(),
                arity: artifact.arity,
                artifact_hash: artifact.artifact_hash.clone(),
            })
            .collect();
        self.publish_bindings(namespace, &bindings)
    }

    pub fn list_namespaces(&self) -> Result<Vec<NamespaceSummary>, ServiceError> {
        Ok(self.store.list_namespaces()?)
    }

    pub fn get_namespace_bindings(
        &self,
        namespace: &str,
        generation: Option<i64>,
    ) -> Result<Option<(i64, Vec<PublishedBinding>)>, ServiceError> {
        Ok(self.store.get_namespace_bindings(namespace, generation)?)
    }

    pub fn get_snapshot_summary(
        &self,
        snapshot_id: i64,
    ) -> Result<Option<SnapshotSummary>, ServiceError> {
        Ok(self.store.get_snapshot_summary(snapshot_id)?)
    }

    pub fn diff_namespace_generations(
        &self,
        namespace: &str,
        from_generation: i64,
        to_generation: i64,
    ) -> Result<Option<NamespaceDiff>, ServiceError> {
        Ok(self
            .store
            .diff_namespace_generations(namespace, from_generation, to_generation)?)
    }

    pub fn insert_deployment(&mut self, deployment: &DeploymentRecord) -> Result<(), ServiceError> {
        self.store.insert_deployment(deployment)?;
        Ok(())
    }

    pub fn insert_instance(&mut self, instance: &InstanceRecord) -> Result<(), ServiceError> {
        self.store.insert_instance(instance)?;
        Ok(())
    }

    pub fn update_instance_state(
        &mut self,
        instance_id: &str,
        status: &str,
        stopped_at_ms: Option<i64>,
        exit_code: Option<i32>,
    ) -> Result<(), ServiceError> {
        self.store
            .update_instance_state(instance_id, status, stopped_at_ms, exit_code)?;
        Ok(())
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceRecord>, ServiceError> {
        Ok(self.store.list_instances()?)
    }

    pub fn get_instance(&self, instance_id: &str) -> Result<Option<InstanceRecord>, ServiceError> {
        Ok(self.store.get_instance(instance_id)?)
    }

    pub fn resolve_execution_target(
        &self,
        snapshot_id: i64,
        selector: &str,
    ) -> Result<Option<ExecutionTarget>, ServiceError> {
        if let Some((artifact_hash, arity, revision_id)) =
            self.store.resolve_snapshot_binding(snapshot_id, selector)?
        {
            let capabilities = revision_id
                .as_deref()
                .map(|revision_id| self.store.get_revision_capabilities(revision_id))
                .transpose()?
                .unwrap_or_default();
            return Ok(Some(ExecutionTarget {
                artifact_hash,
                arity,
                revision_id,
                capabilities,
            }));
        }

        Ok(self
            .store
            .get_artifact(selector)?
            .map(|artifact| ExecutionTarget {
                artifact_hash: artifact.artifact_hash,
                arity: artifact.arity,
                revision_id: None,
                capabilities: Vec::new(),
            }))
    }

    pub fn execute_target(
        &self,
        target: &ExecutionTarget,
        args: &[String],
        output_dir: &Path,
    ) -> Result<ExecutionResult, ServiceError> {
        self.execute_target_with_limits(target, args, output_dir, ExecutionLimits::default())
    }

    pub fn execute_target_with_limits(
        &self,
        target: &ExecutionTarget,
        args: &[String],
        output_dir: &Path,
        limits: ExecutionLimits,
    ) -> Result<ExecutionResult, ServiceError> {
        self.execute_target_with_limits_and_resource_cipher(target, args, output_dir, limits, None)
    }

    pub(crate) fn execute_target_with_limits_and_resource_cipher(
        &self,
        target: &ExecutionTarget,
        args: &[String],
        output_dir: &Path,
        limits: ExecutionLimits,
        resource_cipher: Option<&resource::ResourceCipher>,
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

        self.materialize_artifact_closure(&target.artifact_hash, output_dir)?;
        self.compile_materialized_artifacts(output_dir)?;
        if !target.capabilities.is_empty() {
            capability::prepare_bridge(output_dir)?;
        }

        let capability_setup = if target.capabilities.is_empty() {
            String::new()
        } else {
            capability::render_grant_setup(&target.capabilities)
        };

        let eval = if args.is_empty() {
            format!(
                "{}case timer:tc(fun() -> '{}':apply() end) of {{Micros, Value}} -> io:format(\"~p~n__LUX_TC_US__:~B~n\", [Value, Micros]), halt() end.",
                capability_setup, target.artifact_hash
            )
        } else {
            format!(
                "{}case timer:tc(fun() -> '{}':apply({}) end) of {{Micros, Value}} -> io:format(\"~p~n__LUX_TC_US__:~B~n\", [Value, Micros]), halt() end.",
                capability_setup,
                target.artifact_hash,
                args.join(", ")
            )
        };
        let mut command = Command::new("erl");
        command
            .arg("-noshell")
            .arg("-pa")
            .arg(output_dir)
            .arg("-eval")
            .arg(eval)
            .env("LUX_CAPABILITY_HELPER", capability::helper_executable()?);
        if let Some(path) = self.store.path() {
            command.env("LUX_DATABASE_PATH", path);
        }
        if let Some(cipher) = resource_cipher {
            command.env("LUX_MASTER_KEY_HEX", cipher.key_hex());
        }
        let output = run_command_with_limits(command, limits, None)?;
        if !output.status.success() {
            return Err(ServiceError::Io(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            )));
        }

        let (stdout, beam_time_us) = parse_execution_stdout(&output.stdout);

        Ok(ExecutionResult {
            stdout,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            beam_time_us,
        })
    }

    pub fn prepare_target_runtime(
        &self,
        target: &ExecutionTarget,
        output_dir: &Path,
    ) -> Result<(), ServiceError> {
        self.materialize_artifact_closure(&target.artifact_hash, output_dir)?;
        self.compile_materialized_artifacts(output_dir)?;
        Ok(())
    }

    pub fn prepare_shared_artifact_cache(
        &self,
        root_hash: &str,
        cache_dir: &Path,
    ) -> Result<(), ServiceError> {
        fs::create_dir_all(cache_dir)?;
        let mut pending = vec![root_hash.to_string()];
        let mut seen = std::collections::HashSet::new();
        let mut compile_queue = Vec::new();

        while let Some(hash) = pending.pop() {
            if !seen.insert(hash.clone()) {
                continue;
            }
            let artifact = self
                .store
                .get_artifact(&hash)?
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, hash.clone()))?;
            let core_path = cache_dir.join(format!("{}.core", artifact.artifact_hash));
            let beam_path = cache_dir.join(format!("{}.beam", artifact.artifact_hash));
            let build_stamp_path = cache_dir.join(format!("{}.beam.stamp", artifact.build_key));

            if !core_path.exists() {
                fs::write(&core_path, &artifact.core_source)?;
            }
            if !beam_path.exists() || !build_stamp_path.exists() {
                compile_queue.push((core_path, build_stamp_path));
            }
            for dependency in artifact.dependencies {
                pending.push(dependency);
            }
        }

        let core_files: Vec<std::path::PathBuf> =
            compile_queue.iter().map(|(path, _)| path.clone()).collect();
        compile_core_files(&core_files)?;
        for (_, build_stamp_path) in compile_queue {
            fs::write(build_stamp_path, [])?;
        }
        Ok(())
    }

    pub fn entry_target(&self, package: &CompiledPackage) -> Result<ExecutionTarget, ServiceError> {
        match (&package.entry_module, package.entry_arity) {
            (Some(artifact_hash), Some(arity)) => Ok(ExecutionTarget {
                artifact_hash: artifact_hash.clone(),
                arity,
                revision_id: None,
                capabilities: Vec::new(),
            }),
            _ => Err(ServiceError::MissingEntryPoint),
        }
    }

    pub fn evaluate_source_in_snapshot(
        &mut self,
        source: &str,
        snapshot_id: i64,
        config: SessionConfig,
        args: &[String],
        output_dir: &Path,
    ) -> Result<EvaluatedPackage, ServiceError> {
        self.evaluate_source_in_snapshot_with_limits(
            source,
            snapshot_id,
            config,
            args,
            output_dir,
            ExecutionLimits::default(),
        )
    }

    pub fn evaluate_source_in_snapshot_with_limits(
        &mut self,
        source: &str,
        snapshot_id: i64,
        config: SessionConfig,
        args: &[String],
        output_dir: &Path,
        limits: ExecutionLimits,
    ) -> Result<EvaluatedPackage, ServiceError> {
        let package = self.compile_source_in_snapshot(source, snapshot_id, config)?;
        self.store_package(&package)?;
        let target = self.entry_target(&package)?;
        let result = self.execute_target_with_limits(&target, args, output_dir, limits)?;
        Ok(EvaluatedPackage {
            package,
            target,
            result,
        })
    }

    pub fn record_execution(&mut self, record: &ExecutionRecord) -> Result<(), ServiceError> {
        self.store.insert_execution(record)?;
        Ok(())
    }

    fn materialize_artifact_closure(
        &self,
        root_hash: &str,
        output_dir: &Path,
    ) -> Result<(), ServiceError> {
        fs::create_dir_all(output_dir)?;
        let mut pending = vec![root_hash.to_string()];
        let mut seen = std::collections::HashSet::new();

        while let Some(hash) = pending.pop() {
            if !seen.insert(hash.clone()) {
                continue;
            }
            let artifact = self
                .store
                .get_artifact(&hash)?
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, hash.clone()))?;
            let core_path = output_dir.join(format!("{}.core", artifact.artifact_hash));
            fs::write(core_path, artifact.core_source)?;
            for dependency in artifact.dependencies {
                pending.push(dependency);
            }
        }

        Ok(())
    }

    fn compile_materialized_artifacts(&self, output_dir: &Path) -> Result<(), ServiceError> {
        let mut core_files = Vec::new();
        for entry in fs::read_dir(output_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("core") {
                core_files.push(path);
            }
        }
        compile_core_files(&core_files)
    }
}

fn apply_source_edits(source: &str, edits: &[SourceEdit]) -> Result<String, ServiceError> {
    if edits.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "at least one source edit is required".to_string(),
        ));
    }

    let mut ordered = edits.to_vec();
    ordered.sort_by_key(|edit| (edit.start, edit.end));
    let mut previous_end = 0;
    for edit in &ordered {
        if edit.start > edit.end || edit.end > source.len() {
            return Err(ServiceError::InvalidRequest(format!(
                "invalid source edit range {}..{} for {} byte source",
                edit.start,
                edit.end,
                source.len()
            )));
        }
        if !source.is_char_boundary(edit.start) || !source.is_char_boundary(edit.end) {
            return Err(ServiceError::InvalidRequest(format!(
                "source edit range {}..{} is not on UTF-8 boundaries",
                edit.start, edit.end
            )));
        }
        if edit.start < previous_end {
            return Err(ServiceError::InvalidRequest(
                "source edits must not overlap".to_string(),
            ));
        }
        previous_end = edit.end;
    }

    let mut result = source.to_string();
    for edit in ordered.iter().rev() {
        result.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(result)
}

fn canonicalize_capabilities(capabilities: &[String]) -> Result<Vec<String>, ServiceError> {
    let mut canonical = Vec::with_capacity(capabilities.len());
    for capability in capabilities {
        let capability = capability.trim();
        if capability.is_empty()
            || capability.len() > 128
            || !capability.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
        {
            return Err(ServiceError::InvalidRequest(format!(
                "invalid capability name: {capability:?}"
            )));
        }
        canonical.push(capability.to_string());
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

fn validate_resource_identity(namespace: &str, name: &str, kind: &str) -> Result<(), ServiceError> {
    for (label, value) in [
        ("namespace", namespace),
        ("resource name", name),
        ("kind", kind),
    ] {
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
        {
            return Err(ServiceError::InvalidRequest(format!(
                "invalid {label}: {value:?}"
            )));
        }
    }
    Ok(())
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn compile_core_files(core_files: &[std::path::PathBuf]) -> Result<(), ServiceError> {
    if core_files.is_empty() {
        return Ok(());
    }

    let mut command = Command::new("erlc");
    command.arg("+from_core");
    for core_file in core_files {
        command.arg(core_file);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(ServiceError::Io(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )));
    }
    Ok(())
}

pub(crate) fn parse_execution_stdout(stdout: &[u8]) -> (String, Option<u64>) {
    const MARKER: &str = "__LUX_TC_US__:";

    let rendered = String::from_utf8_lossy(stdout).trim().to_string();
    let Some((value, timing_line)) = rendered.rsplit_once('\n') else {
        return (rendered, None);
    };
    let Some(timing) = timing_line.strip_prefix(MARKER) else {
        return (rendered, None);
    };
    let Ok(beam_time_us) = timing.parse::<u64>() else {
        return (rendered, None);
    };
    (value.trim().to_string(), Some(beam_time_us))
}

pub(crate) struct CommandOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

struct CapturedStream {
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ParsedNamespaceSymbol {
    kind: NamespaceSymbolKind,
    symbol: String,
    arity: usize,
    declaration_kind: String,
    source: String,
}

struct ParsedNamespaceSource {
    symbols: Vec<ParsedNamespaceSymbol>,
    compile_context: String,
}

#[allow(clippy::too_many_arguments)]
fn make_namespace_symbol_revision(
    namespace: &str,
    kind: NamespaceSymbolKind,
    symbol: &str,
    arity: usize,
    declaration_kind: &str,
    source: &str,
    compile_context: &str,
    parent_revision_id: Option<&str>,
    base_generation: i64,
    sandbox: bool,
    capabilities: Vec<String>,
    provenance_revision_id: Option<String>,
) -> NamespaceSymbolRevision {
    let source_hash = hash_str(source);
    let compile_context_identity =
        if parent_revision_id.is_some() || provenance_revision_id.is_none() {
            hash_str(compile_context)
        } else {
            String::new()
        };
    let identity = format!(
        "namespace-symbol-revision:{namespace}:{}:{symbol}:{arity}:{declaration_kind}:{}:{base_generation}:{sandbox}:{}:{source_hash}:{compile_context_identity}",
        kind.as_str(),
        parent_revision_id.unwrap_or(""),
        capabilities.join("\u{1f}"),
    );
    NamespaceSymbolRevision {
        revision_id: hash_str(&identity),
        namespace: namespace.to_string(),
        kind,
        symbol: symbol.to_string(),
        arity,
        declaration_kind: declaration_kind.to_string(),
        parent_revision_id: parent_revision_id.map(str::to_string),
        base_generation,
        source_hash,
        source: source.to_string(),
        compile_context: compile_context.to_string(),
        sandbox,
        capabilities,
        provenance_revision_id,
        created_at_ms: unix_time_ms(),
    }
}

fn symbol_compile_config(sandbox: bool, capabilities: &[String]) -> SessionConfig {
    if sandbox && !capabilities.is_empty() {
        SessionConfig::capability_sandboxed()
    } else if sandbox {
        SessionConfig::sandboxed_default()
    } else {
        SessionConfig::trusted()
    }
}

fn namespace_compile_config(symbols: &[&NamespaceSymbol]) -> SessionConfig {
    if symbols.iter().any(|symbol| !symbol.revision.sandbox) {
        SessionConfig::trusted()
    } else if symbols
        .iter()
        .any(|symbol| !symbol.revision.capabilities.is_empty())
    {
        SessionConfig::capability_sandboxed()
    } else {
        SessionConfig::sandboxed_default()
    }
}

fn parse_namespace_source(source: &str) -> Result<ParsedNamespaceSource, ServiceError> {
    let module = Parser::new(Lexer::new(source).tokenize())
        .parse_module()
        .map_err(|error| {
            ServiceError::InvalidRequest(format!(
                "invalid Lux source at bytes {}..{}: {}",
                error.span.start, error.span.end, error.message
            ))
        })?;
    let mut symbols = Vec::new();
    let mut identities = HashSet::new();
    let mut context = Vec::new();
    for item in module.items {
        let (kind, symbol, arity, declaration_kind, span) = match item {
            Item::Function(function) => (
                NamespaceSymbolKind::Function,
                function.name,
                function.params.len(),
                "function",
                function.span,
            ),
            Item::TypeAlias(alias) => (
                NamespaceSymbolKind::Type,
                alias.name,
                alias.type_params.len(),
                "alias",
                alias.span,
            ),
            Item::Enum(definition) => (
                NamespaceSymbolKind::Type,
                definition.name,
                definition.type_params.len(),
                "enum",
                definition.span,
            ),
            Item::Struct(definition) => (
                NamespaceSymbolKind::Type,
                definition.name,
                definition.type_params.len(),
                "struct",
                definition.span,
            ),
            Item::Extern(block) => {
                let item_source = source
                    .get(block.span.start as usize..block.span.end as usize)
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest("invalid extern declaration span".to_string())
                    })?;
                context.push(item_source.to_string());
                continue;
            }
            Item::Use(declaration) => {
                let item_source = source
                    .get(declaration.span.start as usize..declaration.span.end as usize)
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest("invalid use declaration span".to_string())
                    })?;
                context.push(item_source.to_string());
                continue;
            }
        };
        let item_source = source
            .get(span.start as usize..span.end as usize)
            .ok_or_else(|| {
                ServiceError::InvalidRequest(format!(
                    "invalid parser span {}..{} for {} byte source",
                    span.start,
                    span.end,
                    source.len()
                ))
            })?;
        let identity = (kind.as_str().to_string(), symbol.clone());
        if !identities.insert(identity) {
            return Err(ServiceError::InvalidRequest(format!(
                "namespace defines symbol `{symbol}` more than once; a symbol name has exactly one definition"
            )));
        }
        symbols.push(ParsedNamespaceSymbol {
            kind,
            symbol,
            arity,
            declaration_kind: declaration_kind.to_string(),
            source: item_source.to_string(),
        });
    }
    context.sort();
    context.dedup();
    Ok(ParsedNamespaceSource {
        symbols,
        compile_context: context.join("\n\n"),
    })
}

fn validate_single_symbol_source(
    source: &str,
    kind: &NamespaceSymbolKind,
    symbol: &str,
) -> Result<(ParsedNamespaceSymbol, String), ServiceError> {
    let mut parsed_source = parse_namespace_source(source)?;
    if parsed_source.symbols.len() != 1 {
        return Err(ServiceError::InvalidRequest(
            "a symbol edit must contain exactly one function or type definition".to_string(),
        ));
    }
    let parsed = parsed_source.symbols.remove(0);
    if parsed.kind != *kind || parsed.symbol != symbol {
        return Err(ServiceError::InvalidRequest(format!(
            "source defines {}/{} instead of {}/{}",
            parsed.kind.as_str(),
            parsed.symbol,
            kind.as_str(),
            symbol
        )));
    }
    Ok((parsed, parsed_source.compile_context))
}

pub(crate) fn run_command_with_limits(
    mut command: Command,
    limits: ExecutionLimits,
    stdin_bytes: Option<Vec<u8>>,
) -> Result<CommandOutput, ServiceError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_bytes.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let stdin_handle = match (stdin_bytes, child.stdin.take()) {
        (Some(stdin_bytes), Some(mut stdin)) => {
            Some(thread::spawn(move || -> std::io::Result<()> {
                stdin.write_all(&stdin_bytes)?;
                stdin.flush()?;
                Ok(())
            }))
        }
        _ => None,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture stderr"))?;

    let total_bytes = Arc::new(AtomicUsize::new(0));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_handle = spawn_capture_thread(
        stdout,
        Arc::clone(&total_bytes),
        Arc::clone(&output_exceeded),
        limits.output_limit_bytes,
    );
    let stderr_handle = spawn_capture_thread(
        stderr,
        total_bytes,
        Arc::clone(&output_exceeded),
        limits.output_limit_bytes,
    );

    let start = Instant::now();
    let status = loop {
        if output_exceeded.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(ServiceError::OutputLimitExceeded {
                limit_bytes: limits.output_limit_bytes,
            });
        }
        if let Some(status) = child.try_wait()? {
            break Ok(status);
        }
        if start.elapsed() >= Duration::from_millis(limits.timeout_ms) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(ServiceError::Timeout {
                timeout_ms: limits.timeout_ms,
            });
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout = join_capture_thread(stdout_handle)?;
    let stderr = join_capture_thread(stderr_handle)?;
    if let Some(handle) = stdin_handle {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(Err(err)) => return Err(ServiceError::Io(err)),
            Err(_) => {
                return Err(ServiceError::Io(std::io::Error::other(
                    "stdin thread panicked",
                )));
            }
        }
    }
    match status {
        Ok(status) => Ok(CommandOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        }),
        Err(err) => Err(err),
    }
}

fn spawn_capture_thread<R: Read + Send + 'static>(
    mut reader: R,
    total_bytes: Arc<AtomicUsize>,
    output_exceeded: Arc<AtomicBool>,
    output_limit_bytes: usize,
) -> thread::JoinHandle<std::io::Result<CapturedStream>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let previous = total_bytes.fetch_add(read, Ordering::Relaxed);
            if previous.saturating_add(read) > output_limit_bytes {
                output_exceeded.store(true, Ordering::Relaxed);
            }
            if bytes.len() < output_limit_bytes {
                let remaining = output_limit_bytes - bytes.len();
                bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            }
        }
        Ok(CapturedStream { bytes })
    })
}

fn join_capture_thread(
    handle: thread::JoinHandle<std::io::Result<CapturedStream>>,
) -> Result<CapturedStream, ServiceError> {
    match handle.join() {
        Ok(result) => result.map_err(ServiceError::Io),
        Err(_) => Err(ServiceError::Io(std::io::Error::other(
            "capture thread panicked",
        ))),
    }
}

fn collect_apply_dependencies(module: &CoreModule, self_hash: &str) -> Vec<String> {
    let mut dependencies = Vec::new();
    for function in &module.functions {
        collect_apply_dependencies_expr(&function.body, self_hash, &mut dependencies);
    }
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

fn is_artifact_dependency_module(module: &str) -> bool {
    module != "erlang"
}

fn collect_apply_dependencies_expr(
    expr: &CoreExpr,
    self_hash: &str,
    dependencies: &mut Vec<String>,
) {
    match expr {
        CoreExpr::Call(module, func, args) => {
            if func == "apply" && module != self_hash && is_artifact_dependency_module(module) {
                dependencies.push(module.clone());
            }
            for arg in args {
                collect_apply_dependencies_expr(arg, self_hash, dependencies);
            }
        }
        CoreExpr::RemoteFunRef(module, func, _) => {
            if func == "apply" && module != self_hash && is_artifact_dependency_module(module) {
                dependencies.push(module.clone());
            }
        }
        CoreExpr::Tuple(items) => {
            for item in items {
                collect_apply_dependencies_expr(item, self_hash, dependencies);
            }
        }
        CoreExpr::Binary(items) => {
            for item in items {
                collect_apply_dependencies_expr(&item.value, self_hash, dependencies);
            }
        }
        CoreExpr::List(items, tail) => {
            for item in items {
                collect_apply_dependencies_expr(item, self_hash, dependencies);
            }
            collect_apply_dependencies_expr(tail, self_hash, dependencies);
        }
        CoreExpr::Cons(head, tail) => {
            collect_apply_dependencies_expr(head, self_hash, dependencies);
            collect_apply_dependencies_expr(tail, self_hash, dependencies);
        }
        CoreExpr::Map(entries) => {
            for (key, value) in entries {
                collect_apply_dependencies_expr(key, self_hash, dependencies);
                collect_apply_dependencies_expr(value, self_hash, dependencies);
            }
        }
        CoreExpr::Apply(func, args) => {
            collect_apply_dependencies_expr(func, self_hash, dependencies);
            for arg in args {
                collect_apply_dependencies_expr(arg, self_hash, dependencies);
            }
        }
        CoreExpr::Let(bindings, body) => {
            for (_, value) in bindings {
                collect_apply_dependencies_expr(value, self_hash, dependencies);
            }
            collect_apply_dependencies_expr(body, self_hash, dependencies);
        }
        CoreExpr::Case(scrutinee, clauses) => {
            collect_apply_dependencies_expr(scrutinee, self_hash, dependencies);
            for clause in clauses {
                collect_apply_dependencies_expr(&clause.guard, self_hash, dependencies);
                collect_apply_dependencies_expr(&clause.body, self_hash, dependencies);
            }
        }
        CoreExpr::Receive { clauses, timeout } => {
            for clause in clauses {
                collect_apply_dependencies_expr(&clause.guard, self_hash, dependencies);
                collect_apply_dependencies_expr(&clause.body, self_hash, dependencies);
            }
            if let Some((timeout_ms, timeout_body)) = timeout {
                collect_apply_dependencies_expr(timeout_ms, self_hash, dependencies);
                collect_apply_dependencies_expr(timeout_body, self_hash, dependencies);
            }
        }
        CoreExpr::Fun(_, body) => {
            collect_apply_dependencies_expr(body, self_hash, dependencies);
        }
        CoreExpr::Primop(_, args) => {
            for arg in args {
                collect_apply_dependencies_expr(arg, self_hash, dependencies);
            }
        }
        CoreExpr::Seq(first, second) => {
            collect_apply_dependencies_expr(first, self_hash, dependencies);
            collect_apply_dependencies_expr(second, self_hash, dependencies);
        }
        CoreExpr::Try {
            body,
            handler,
            catch,
            ..
        } => {
            collect_apply_dependencies_expr(body, self_hash, dependencies);
            collect_apply_dependencies_expr(handler, self_hash, dependencies);
            collect_apply_dependencies_expr(catch, self_hash, dependencies);
        }
        CoreExpr::Lit(_) | CoreExpr::Var(_) | CoreExpr::LocalFunRef(_, _) => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::codegen::erlang::{CoreFunDef, CoreLit};

    use super::*;
    use crate::driver::session::SessionConfig;

    #[test]
    fn compile_against_snapshot_resolves_symbol_to_hash_call() {
        let mut service = LiveCodeService::new(store::SqliteStore::open_in_memory().unwrap());
        let generation = service
            .publish_bindings(
                "dev",
                &[NamespaceBinding {
                    symbol: "fib".to_string(),
                    arity: 1,
                    artifact_hash: "artifact_fib_v1".to_string(),
                }],
            )
            .unwrap();
        let snapshot = service.create_snapshot("dev", Some(generation)).unwrap();

        let package = service
            .compile_source_in_snapshot("fn main() { fib(10) }", snapshot, SessionConfig::trusted())
            .unwrap();

        let entry = package
            .artifacts
            .iter()
            .find(|artifact| artifact.source_name == "main")
            .unwrap();
        assert!(
            entry
                .core_source
                .contains("call 'artifact_fib_v1':'apply'(10)")
        );
        assert_eq!(entry.dependencies, vec!["artifact_fib_v1".to_string()]);
    }

    #[test]
    fn parse_execution_stdout_extracts_beam_timer_value() {
        let (stdout, beam_time_us) = parse_execution_stdout(b"55\n__LUX_TC_US__:1234\n");

        assert_eq!(stdout, "55");
        assert_eq!(beam_time_us, Some(1234));
    }

    #[test]
    fn parse_execution_stdout_without_marker_preserves_output() {
        let (stdout, beam_time_us) = parse_execution_stdout(b"55\n");

        assert_eq!(stdout, "55");
        assert_eq!(beam_time_us, None);
    }

    #[test]
    fn collect_apply_dependencies_ignores_erlang_apply() {
        let module = CoreModule {
            name: "0123456789abcdef".to_string(),
            exports: vec![("apply".to_string(), 0)],
            functions: vec![CoreFunDef {
                name: "apply".to_string(),
                arity: 0,
                params: vec![],
                body: CoreExpr::Call(
                    "erlang".to_string(),
                    "apply".to_string(),
                    vec![
                        CoreExpr::Lit(CoreLit::Atom("json".to_string())),
                        CoreExpr::Lit(CoreLit::Atom("decode".to_string())),
                        CoreExpr::List(
                            vec![CoreExpr::Lit(CoreLit::String("{\"a\":1}".to_string()))],
                            Box::new(CoreExpr::Lit(CoreLit::Nil)),
                        ),
                    ],
                ),
            }],
        };

        assert!(collect_apply_dependencies(&module, "0123456789abcdef").is_empty());
    }
}
