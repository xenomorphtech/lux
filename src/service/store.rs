use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, params, params_from_iter, types::Value as SqlValue,
};

use super::{
    BindingChange, ChangesetBuild, CompiledPackage, DeploymentRecord, ExecutionRecord,
    FunctionArtifact, InstanceRecord, NamespaceBinding, NamespaceBindingUpdate, NamespaceChangeset,
    NamespaceDiff, NamespaceResource, NamespaceRevision, NamespaceSummary, NamespaceSymbol,
    NamespaceSymbolKind, NamespaceSymbolRevision, PublishedBinding, SnapshotSummary,
    StoredNamespaceResource,
};

pub struct SqliteStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        let store = Self {
            conn,
            path: Some(path),
        };
        store.bootstrap_schema()?;
        Ok(store)
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            path: Some(path),
        })
    }

    pub fn open_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn, path: None };
        store.bootstrap_schema()?;
        Ok(store)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn bootstrap_schema(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS artifacts (
                artifact_hash TEXT PRIMARY KEY,
                body_hash TEXT NOT NULL,
                abi_hash TEXT NOT NULL DEFAULT '',
                source_name TEXT NOT NULL,
                arity INTEGER NOT NULL,
                build_key TEXT NOT NULL DEFAULT '',
                core_source TEXT NOT NULL,
                source_module TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS artifact_deps (
                artifact_hash TEXT NOT NULL,
                callee_hash TEXT NOT NULL,
                call_kind TEXT NOT NULL DEFAULT 'apply',
                PRIMARY KEY (artifact_hash, callee_hash)
            );

            CREATE TABLE IF NOT EXISTS namespaces (
                namespace_id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS namespace_generations (
                namespace_id INTEGER NOT NULL,
                generation INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (namespace_id, generation)
            );

            CREATE TABLE IF NOT EXISTS bindings (
                binding_id INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                arity INTEGER NOT NULL,
                artifact_hash TEXT NOT NULL,
                generation INTEGER NOT NULL,
                active INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS snapshots (
                snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id INTEGER NOT NULL,
                generation INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS snapshot_bindings (
                snapshot_id INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                arity INTEGER NOT NULL,
                artifact_hash TEXT NOT NULL,
                revision_id TEXT,
                PRIMARY KEY (snapshot_id, symbol, arity)
            );

            CREATE TABLE IF NOT EXISTS executions (
                execution_id TEXT PRIMARY KEY,
                request_kind TEXT NOT NULL,
                snapshot_id INTEGER NOT NULL,
                entry_artifact_hash TEXT,
                target_selector TEXT,
                source_hash TEXT,
                status TEXT NOT NULL,
                error_message TEXT,
                started_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER NOT NULL,
                wall_time_ms INTEGER NOT NULL,
                stdout_bytes INTEGER NOT NULL,
                stderr_bytes INTEGER NOT NULL,
                stdout_preview TEXT NOT NULL,
                stderr_preview TEXT NOT NULL,
                arg_fingerprint TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_executions_snapshot_started
                ON executions (snapshot_id, started_at_ms);
            CREATE INDEX IF NOT EXISTS idx_executions_entry_started
                ON executions (entry_artifact_hash, started_at_ms);

            CREATE TABLE IF NOT EXISTS deployments (
                deployment_id TEXT PRIMARY KEY,
                snapshot_id INTEGER NOT NULL,
                entry_artifact_hash TEXT NOT NULL,
                target_selector TEXT NOT NULL,
                args_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS instances (
                instance_id TEXT PRIMARY KEY,
                deployment_id TEXT NOT NULL,
                snapshot_id INTEGER NOT NULL,
                entry_artifact_hash TEXT NOT NULL,
                target_selector TEXT NOT NULL,
                args_json TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                stopped_at_ms INTEGER,
                pid INTEGER,
                exit_code INTEGER,
                run_dir TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_instances_status_started
                ON instances (status, started_at_ms);

            CREATE TABLE IF NOT EXISTS namespace_changesets (
                changeset_id INTEGER PRIMARY KEY AUTOINCREMENT,
                namespace_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                head_revision_id TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                UNIQUE (namespace_id, name)
            );

            CREATE TABLE IF NOT EXISTS namespace_revisions (
                revision_id TEXT PRIMARY KEY,
                changeset_id INTEGER NOT NULL,
                parent_revision_id TEXT,
                base_snapshot_id INTEGER,
                source_hash TEXT NOT NULL,
                source TEXT NOT NULL,
                sandbox INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_namespace_revisions_changeset_created
                ON namespace_revisions (changeset_id, created_at_ms);

            CREATE TABLE IF NOT EXISTS namespace_revision_capabilities (
                revision_id TEXT NOT NULL,
                capability TEXT NOT NULL,
                PRIMARY KEY (revision_id, capability)
            );

            CREATE INDEX IF NOT EXISTS idx_revision_capabilities_name
                ON namespace_revision_capabilities (capability, revision_id);

            CREATE TABLE IF NOT EXISTS namespace_symbol_revisions (
                revision_id TEXT PRIMARY KEY,
                namespace_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                symbol TEXT NOT NULL,
                arity INTEGER NOT NULL,
                declaration_kind TEXT NOT NULL,
                parent_revision_id TEXT,
                base_generation INTEGER NOT NULL,
                source_hash TEXT NOT NULL,
                source TEXT NOT NULL,
                compile_context TEXT NOT NULL DEFAULT '',
                sandbox INTEGER NOT NULL DEFAULT 1,
                provenance_revision_id TEXT,
                created_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_namespace_symbol_revisions_identity
                ON namespace_symbol_revisions (namespace_id, kind, symbol, created_at_ms);

            CREATE TABLE IF NOT EXISTS namespace_symbol_revision_capabilities (
                revision_id TEXT NOT NULL,
                capability TEXT NOT NULL,
                PRIMARY KEY (revision_id, capability)
            );

            CREATE TABLE IF NOT EXISTS namespace_symbol_publications (
                namespace_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                symbol TEXT NOT NULL,
                arity INTEGER NOT NULL,
                generation INTEGER NOT NULL,
                revision_id TEXT NOT NULL,
                PRIMARY KEY (namespace_id, kind, symbol, arity, generation)
            );

            CREATE INDEX IF NOT EXISTS idx_namespace_symbol_publications_generation
                ON namespace_symbol_publications (namespace_id, generation);

            CREATE TABLE IF NOT EXISTS namespace_resources (
                resource_id TEXT PRIMARY KEY,
                namespace_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                revision_id TEXT,
                content_hash TEXT NOT NULL,
                nonce BLOB NOT NULL,
                ciphertext BLOB NOT NULL,
                created_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_namespace_resources_name_created
                ON namespace_resources (namespace_id, name, created_at_ms);

            CREATE TABLE IF NOT EXISTS namespace_resource_heads (
                namespace_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                resource_id TEXT NOT NULL,
                PRIMARY KEY (namespace_id, name)
            );

            CREATE TABLE IF NOT EXISTS changeset_builds (
                build_id TEXT PRIMARY KEY,
                revision_id TEXT NOT NULL,
                entry_artifact_hash TEXT,
                entry_arity INTEGER,
                artifact_count INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS changeset_build_artifacts (
                build_id TEXT NOT NULL,
                artifact_hash TEXT NOT NULL,
                source_name TEXT NOT NULL,
                arity INTEGER NOT NULL,
                PRIMARY KEY (build_id, artifact_hash)
            );
            ",
        )?;
        self.ensure_column("artifacts", "abi_hash", "TEXT NOT NULL DEFAULT ''")?;
        self.ensure_column("artifacts", "build_key", "TEXT NOT NULL DEFAULT ''")?;
        self.ensure_column("bindings", "revision_id", "TEXT")?;
        self.ensure_column("snapshot_bindings", "revision_id", "TEXT")?;
        self.ensure_column(
            "namespace_symbol_revisions",
            "compile_context",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        self.conn.execute_batch(
            "
            DROP INDEX IF EXISTS idx_namespace_symbol_revisions_identity;
            CREATE INDEX idx_namespace_symbol_revisions_identity
                ON namespace_symbol_revisions (namespace_id, kind, symbol, created_at_ms);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_namespace_symbol_publication_identity
                ON namespace_symbol_publications (namespace_id, kind, symbol, generation);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_binding_symbol_generation
                ON bindings (namespace_id, symbol, generation) WHERE active = 1;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_snapshot_binding_symbol
                ON snapshot_bindings (snapshot_id, symbol);
            CREATE INDEX IF NOT EXISTS idx_bindings_revision
                ON bindings (revision_id);
            INSERT OR IGNORE INTO namespace_generations (namespace_id, generation, created_at_ms)
                SELECT namespace_id, generation, 0 FROM bindings GROUP BY namespace_id, generation;
            ",
        )?;
        Ok(())
    }

    pub fn insert_package(&mut self, package: &CompiledPackage) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        for artifact in &package.artifacts {
            Self::insert_artifact_tx(&tx, &package.source_module, artifact)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn publish_bindings(
        &mut self,
        namespace: &str,
        bindings: &[NamespaceBinding],
    ) -> Result<i64, rusqlite::Error> {
        self.publish_bindings_with_revision(namespace, bindings, None)
    }

    pub fn publish_bindings_with_revision(
        &mut self,
        namespace: &str,
        bindings: &[NamespaceBinding],
        revision_id: Option<&str>,
    ) -> Result<i64, rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, namespace)?;
        let previous_generation = Self::latest_generation_tx(&tx, namespace_id)?;
        let generation = Self::next_generation_tx(&tx, namespace_id)?;
        tx.execute(
            "
            INSERT INTO namespace_generations (namespace_id, generation, created_at_ms)
            VALUES (?1, ?2, ?3)
            ",
            params![namespace_id, generation, current_time_ms()],
        )?;

        if let Some(previous_generation) = previous_generation {
            let replaced: std::collections::HashSet<String> = bindings
                .iter()
                .map(|binding| binding.symbol.clone())
                .collect();
            let mut stmt = tx.prepare(
                "
                SELECT symbol, arity, artifact_hash, revision_id
                FROM bindings
                WHERE namespace_id = ?1 AND generation = ?2 AND active = 1
                ",
            )?;
            let rows = stmt.query_map(params![namespace_id, previous_generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (symbol, arity, artifact_hash, source_revision_id) = row?;
                if replaced.contains(&symbol) {
                    continue;
                }
                tx.execute(
                    "
                    INSERT INTO bindings (
                        namespace_id, symbol, arity, artifact_hash, generation, active, revision_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                    ",
                    params![
                        namespace_id,
                        symbol,
                        arity,
                        artifact_hash,
                        generation,
                        source_revision_id
                    ],
                )?;
            }
        }

        for binding in bindings {
            tx.execute(
                "
                INSERT INTO bindings (
                    namespace_id, symbol, arity, artifact_hash, generation, active, revision_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                ",
                params![
                    namespace_id,
                    binding.symbol,
                    binding.arity as i64,
                    binding.artifact_hash,
                    generation,
                    revision_id
                ],
            )?;
        }
        tx.commit()?;
        Ok(generation)
    }

    pub fn publish_symbol_update(
        &mut self,
        namespace: &str,
        expected_generation: i64,
        bindings: &[NamespaceBindingUpdate],
        revision: &NamespaceSymbolRevision,
    ) -> Result<i64, rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, namespace)?;
        let actual_generation = Self::latest_generation_tx(&tx, namespace_id)?.unwrap_or(0);
        if actual_generation != expected_generation {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "namespace generation conflict: expected {expected_generation}, current {actual_generation}"
            )));
        }
        let generation = actual_generation + 1;
        tx.execute(
            "
            INSERT INTO namespace_generations (namespace_id, generation, created_at_ms)
            VALUES (?1, ?2, ?3)
            ",
            params![namespace_id, generation, revision.created_at_ms],
        )?;

        let replaced = bindings
            .iter()
            .map(|update| update.binding.symbol.clone())
            .collect::<std::collections::HashSet<_>>();
        if actual_generation > 0 {
            let mut stmt = tx.prepare(
                "
                SELECT symbol, arity, artifact_hash, revision_id
                FROM bindings
                WHERE namespace_id = ?1 AND generation = ?2 AND active = 1
                ",
            )?;
            let rows = stmt.query_map(params![namespace_id, actual_generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (symbol, arity, artifact_hash, source_revision_id) = row?;
                if replaced.contains(&symbol) {
                    continue;
                }
                tx.execute(
                    "
                    INSERT INTO bindings (
                        namespace_id, symbol, arity, artifact_hash, generation, active, revision_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                    ",
                    params![
                        namespace_id,
                        symbol,
                        arity,
                        artifact_hash,
                        generation,
                        source_revision_id,
                    ],
                )?;
            }
        }

        for update in bindings {
            tx.execute(
                "
                INSERT INTO bindings (
                    namespace_id, symbol, arity, artifact_hash, generation, active, revision_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                ",
                params![
                    namespace_id,
                    update.binding.symbol,
                    update.binding.arity as i64,
                    update.binding.artifact_hash,
                    generation,
                    update.revision_id,
                ],
            )?;
        }
        tx.execute(
            "
            INSERT INTO namespace_symbol_publications
                (namespace_id, kind, symbol, arity, generation, revision_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                namespace_id,
                revision.kind.as_str(),
                revision.symbol,
                revision.arity as i64,
                generation,
                revision.revision_id,
            ],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    pub fn insert_changeset_revision(
        &mut self,
        revision: &NamespaceRevision,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, &revision.namespace)?;
        tx.execute(
            "
            INSERT OR IGNORE INTO namespace_changesets (
                namespace_id, name, head_revision_id, created_at_ms, updated_at_ms
            ) VALUES (?1, ?2, NULL, ?3, ?3)
            ",
            params![namespace_id, revision.changeset, revision.created_at_ms],
        )?;
        let changeset_id = tx.query_row(
            "SELECT changeset_id FROM namespace_changesets WHERE namespace_id = ?1 AND name = ?2",
            params![namespace_id, revision.changeset],
            |row| row.get::<_, i64>(0),
        )?;
        for capability in &revision.capabilities {
            tx.execute(
                "
                INSERT INTO namespace_revision_capabilities (revision_id, capability)
                VALUES (?1, ?2)
                ",
                params![revision.revision_id, capability],
            )?;
        }
        tx.execute(
            "
            INSERT INTO namespace_revisions (
                revision_id, changeset_id, parent_revision_id, base_snapshot_id,
                source_hash, source, sandbox, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                revision.revision_id,
                changeset_id,
                revision.parent_revision_id,
                revision.base_snapshot_id,
                revision.source_hash,
                revision.source,
                i64::from(revision.sandbox),
                revision.created_at_ms,
            ],
        )?;
        tx.execute(
            "
            UPDATE namespace_changesets
            SET head_revision_id = ?2, updated_at_ms = ?3
            WHERE changeset_id = ?1
            ",
            params![changeset_id, revision.revision_id, revision.created_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_changeset(
        &self,
        namespace: &str,
        changeset: &str,
    ) -> Result<Option<NamespaceChangeset>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT c.changeset_id, c.head_revision_id,
                       (SELECT COUNT(*) FROM namespace_revisions r WHERE r.changeset_id = c.changeset_id),
                       c.created_at_ms, c.updated_at_ms
                FROM namespace_changesets c
                JOIN namespaces n ON n.namespace_id = c.namespace_id
                WHERE n.name = ?1 AND c.name = ?2
                ",
                params![namespace, changeset],
                |row| {
                    Ok(NamespaceChangeset {
                        changeset_id: row.get(0)?,
                        namespace: namespace.to_string(),
                        name: changeset.to_string(),
                        head_revision_id: row.get(1)?,
                        revision_count: row.get(2)?,
                        created_at_ms: row.get(3)?,
                        updated_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn list_changesets(
        &self,
        namespace: Option<&str>,
    ) -> Result<Vec<NamespaceChangeset>, rusqlite::Error> {
        let mut query = String::from(
            "
            SELECT n.name, c.name, c.changeset_id, c.head_revision_id,
                   (SELECT COUNT(*) FROM namespace_revisions r WHERE r.changeset_id = c.changeset_id),
                   c.created_at_ms, c.updated_at_ms
            FROM namespace_changesets c
            JOIN namespaces n ON n.namespace_id = c.namespace_id
            ",
        );
        if namespace.is_some() {
            query.push_str(" WHERE n.name = ?1");
        }
        query.push_str(" ORDER BY n.name, c.name");
        let mut stmt = self.conn.prepare(&query)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(NamespaceChangeset {
                namespace: row.get(0)?,
                name: row.get(1)?,
                changeset_id: row.get(2)?,
                head_revision_id: row.get(3)?,
                revision_count: row.get(4)?,
                created_at_ms: row.get(5)?,
                updated_at_ms: row.get(6)?,
            })
        };
        let rows = match namespace {
            Some(namespace) => stmt.query_map(params![namespace], map_row)?,
            None => stmt.query_map([], map_row)?,
        };
        let mut changesets = Vec::new();
        for row in rows {
            changesets.push(row?);
        }
        Ok(changesets)
    }

    pub fn get_changeset_revision(
        &self,
        namespace: &str,
        changeset: &str,
        revision_id: Option<&str>,
    ) -> Result<Option<NamespaceRevision>, rusqlite::Error> {
        let revision = self
            .conn
            .query_row(
                "
                SELECT r.revision_id, r.parent_revision_id, r.base_snapshot_id,
                       r.source_hash, r.source, r.sandbox, r.created_at_ms
                FROM namespace_revisions r
                JOIN namespace_changesets c ON c.changeset_id = r.changeset_id
                JOIN namespaces n ON n.namespace_id = c.namespace_id
                WHERE n.name = ?1 AND c.name = ?2
                  AND r.revision_id = COALESCE(?3, c.head_revision_id)
                ",
                params![namespace, changeset, revision_id],
                |row| {
                    Ok(NamespaceRevision {
                        revision_id: row.get(0)?,
                        namespace: namespace.to_string(),
                        changeset: changeset.to_string(),
                        parent_revision_id: row.get(1)?,
                        base_snapshot_id: row.get(2)?,
                        source_hash: row.get(3)?,
                        source: row.get(4)?,
                        sandbox: row.get::<_, i64>(5)? != 0,
                        capabilities: Vec::new(),
                        created_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?;
        match revision {
            Some(mut revision) => {
                revision.capabilities = self.get_revision_capabilities(&revision.revision_id)?;
                Ok(Some(revision))
            }
            None => Ok(None),
        }
    }

    pub fn get_changeset_revision_by_id(
        &self,
        revision_id: &str,
    ) -> Result<Option<NamespaceRevision>, rusqlite::Error> {
        let revision = self
            .conn
            .query_row(
                "
                SELECT n.name, c.name, r.revision_id, r.parent_revision_id,
                       r.base_snapshot_id, r.source_hash, r.source, r.sandbox, r.created_at_ms
                FROM namespace_revisions r
                JOIN namespace_changesets c ON c.changeset_id = r.changeset_id
                JOIN namespaces n ON n.namespace_id = c.namespace_id
                WHERE r.revision_id = ?1
                ",
                params![revision_id],
                |row| {
                    Ok(NamespaceRevision {
                        namespace: row.get(0)?,
                        changeset: row.get(1)?,
                        revision_id: row.get(2)?,
                        parent_revision_id: row.get(3)?,
                        base_snapshot_id: row.get(4)?,
                        source_hash: row.get(5)?,
                        source: row.get(6)?,
                        sandbox: row.get::<_, i64>(7)? != 0,
                        capabilities: Vec::new(),
                        created_at_ms: row.get(8)?,
                    })
                },
            )
            .optional()?;
        match revision {
            Some(mut revision) => {
                revision.capabilities = self.get_revision_capabilities(&revision.revision_id)?;
                Ok(Some(revision))
            }
            None => Ok(None),
        }
    }

    pub fn get_revision_capabilities(
        &self,
        revision_id: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT capability FROM namespace_revision_capabilities WHERE revision_id = ?1
            UNION
            SELECT capability FROM namespace_symbol_revision_capabilities WHERE revision_id = ?1
            ORDER BY capability
            ",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    pub fn insert_symbol_revision(
        &mut self,
        revision: &NamespaceSymbolRevision,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, &revision.namespace)?;
        tx.execute(
            "
            INSERT OR IGNORE INTO namespace_symbol_revisions (
                revision_id, namespace_id, kind, symbol, arity, declaration_kind,
                parent_revision_id, base_generation, source_hash, source, sandbox,
                compile_context, provenance_revision_id, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            ON CONFLICT(revision_id) DO UPDATE SET
                compile_context = CASE
                    WHEN namespace_symbol_revisions.compile_context = ''
                    THEN excluded.compile_context
                    ELSE namespace_symbol_revisions.compile_context
                END,
                created_at_ms = CASE
                    WHEN namespace_symbol_revisions.provenance_revision_id IS NOT NULL
                    THEN excluded.created_at_ms
                    ELSE namespace_symbol_revisions.created_at_ms
                END
            ",
            params![
                revision.revision_id,
                namespace_id,
                revision.kind.as_str(),
                revision.symbol,
                revision.arity as i64,
                revision.declaration_kind,
                revision.parent_revision_id,
                revision.base_generation,
                revision.source_hash,
                revision.source,
                i64::from(revision.sandbox),
                revision.compile_context,
                revision.provenance_revision_id,
                revision.created_at_ms,
            ],
        )?;
        for capability in &revision.capabilities {
            tx.execute(
                "
                INSERT OR IGNORE INTO namespace_symbol_revision_capabilities
                    (revision_id, capability) VALUES (?1, ?2)
                ",
                params![revision.revision_id, capability],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn publish_symbol_at_generation(
        &mut self,
        revision: &NamespaceSymbolRevision,
        generation: i64,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, &revision.namespace)?;
        tx.execute(
            "
            INSERT OR IGNORE INTO namespace_generations
                (namespace_id, generation, created_at_ms) VALUES (?1, ?2, ?3)
            ",
            params![namespace_id, generation, revision.created_at_ms],
        )?;
        let existing = tx
            .query_row(
                "
                SELECT p.revision_id, r.created_at_ms
                FROM namespace_symbol_publications p
                JOIN namespace_symbol_revisions r ON r.revision_id = p.revision_id
                WHERE p.namespace_id = ?1 AND p.kind = ?2 AND p.symbol = ?3
                  AND p.generation = ?4
                ",
                params![
                    namespace_id,
                    revision.kind.as_str(),
                    revision.symbol,
                    generation,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((existing_revision_id, existing_created_at_ms)) = existing {
            if existing_revision_id == revision.revision_id {
                tx.commit()?;
                return Ok(());
            }
            if (revision.created_at_ms, revision.revision_id.as_str())
                > (existing_created_at_ms, existing_revision_id.as_str())
            {
                tx.execute(
                    "
                    UPDATE namespace_symbol_publications SET arity = ?4, revision_id = ?6
                    WHERE namespace_id = ?1 AND kind = ?2 AND symbol = ?3
                      AND generation = ?5
                    ",
                    params![
                        namespace_id,
                        revision.kind.as_str(),
                        revision.symbol,
                        revision.arity as i64,
                        generation,
                        revision.revision_id,
                    ],
                )?;
            }
            tx.commit()?;
            return Ok(());
        }
        tx.execute(
            "
            INSERT INTO namespace_symbol_publications
                (namespace_id, kind, symbol, arity, generation, revision_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                namespace_id,
                revision.kind.as_str(),
                revision.symbol,
                revision.arity as i64,
                generation,
                revision.revision_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_namespace_symbols(
        &self,
        namespace: &str,
        generation: i64,
    ) -> Result<Vec<NamespaceSymbol>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT r.revision_id, r.kind, r.symbol, r.arity, r.declaration_kind,
                   r.parent_revision_id, r.base_generation, r.source_hash, r.source,
                   r.compile_context, r.sandbox, r.provenance_revision_id, r.created_at_ms,
                   p.generation,
                   CASE WHEN r.kind = 'function' THEN (
                       SELECT b.artifact_hash FROM bindings b
                       WHERE b.namespace_id = r.namespace_id
                         AND b.generation = ?2
                         AND b.symbol = r.symbol AND b.arity = r.arity AND b.active = 1
                   ) ELSE NULL END
            FROM namespace_symbol_publications p
            JOIN namespace_symbol_revisions r ON r.revision_id = p.revision_id
            JOIN namespaces n ON n.namespace_id = p.namespace_id
            WHERE n.name = ?1 AND p.generation = (
                SELECT MAX(p2.generation)
                FROM namespace_symbol_publications p2
                WHERE p2.namespace_id = p.namespace_id
                  AND p2.kind = p.kind AND p2.symbol = p.symbol
                  AND p2.generation <= ?2
            )
            ORDER BY r.kind, r.symbol, r.arity
            ",
        )?;
        let rows = stmt.query_map(params![namespace, generation], |row| {
            let kind = match row.get::<_, String>(1)?.as_str() {
                "function" => NamespaceSymbolKind::Function,
                "type" => NamespaceSymbolKind::Type,
                other => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        format!("invalid namespace symbol kind {other}").into(),
                    ));
                }
            };
            Ok(NamespaceSymbol {
                revision: NamespaceSymbolRevision {
                    revision_id: row.get(0)?,
                    namespace: namespace.to_string(),
                    kind,
                    symbol: row.get(2)?,
                    arity: row.get::<_, i64>(3)? as usize,
                    declaration_kind: row.get(4)?,
                    parent_revision_id: row.get(5)?,
                    base_generation: row.get(6)?,
                    source_hash: row.get(7)?,
                    source: row.get(8)?,
                    compile_context: row.get(9)?,
                    sandbox: row.get::<_, i64>(10)? != 0,
                    capabilities: Vec::new(),
                    provenance_revision_id: row.get(11)?,
                    created_at_ms: row.get(12)?,
                },
                published_generation: row.get(13)?,
                artifact_hash: row.get(14)?,
            })
        })?;
        let mut symbols = Vec::new();
        for row in rows {
            let mut symbol = row?;
            symbol.revision.capabilities =
                self.get_revision_capabilities(&symbol.revision.revision_id)?;
            symbols.push(symbol);
        }
        Ok(symbols)
    }

    pub fn insert_changeset_build(
        &mut self,
        build: &ChangesetBuild,
        package: &CompiledPackage,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "
            INSERT OR REPLACE INTO changeset_builds (
                build_id, revision_id, entry_artifact_hash, entry_arity,
                artifact_count, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                build.build_id,
                build.revision_id,
                build.entry_artifact_hash,
                build.entry_arity.map(|value| value as i64),
                build.artifact_count,
                build.created_at_ms,
            ],
        )?;
        tx.execute(
            "DELETE FROM changeset_build_artifacts WHERE build_id = ?1",
            params![build.build_id],
        )?;
        for artifact in &package.artifacts {
            tx.execute(
                "
                INSERT INTO changeset_build_artifacts (
                    build_id, artifact_hash, source_name, arity
                ) VALUES (?1, ?2, ?3, ?4)
                ",
                params![
                    build.build_id,
                    artifact.artifact_hash,
                    artifact.source_name,
                    artifact.arity as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_namespace_resource(
        &mut self,
        resource: &StoredNamespaceResource,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, &resource.metadata.namespace)?;
        tx.execute(
            "
            INSERT INTO namespace_resources (
                resource_id, namespace_id, name, kind, revision_id,
                content_hash, nonce, ciphertext, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                resource.metadata.resource_id,
                namespace_id,
                resource.metadata.name,
                resource.metadata.kind,
                resource.metadata.revision_id,
                resource.metadata.content_hash,
                resource.nonce,
                resource.ciphertext,
                resource.metadata.created_at_ms,
            ],
        )?;
        tx.execute(
            "
            INSERT INTO namespace_resource_heads (namespace_id, name, resource_id)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(namespace_id, name) DO UPDATE SET resource_id = excluded.resource_id
            ",
            params![
                namespace_id,
                resource.metadata.name,
                resource.metadata.resource_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_namespace_resource(
        &self,
        namespace: &str,
        name: &str,
        resource_id: Option<&str>,
    ) -> Result<Option<StoredNamespaceResource>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT r.resource_id, r.kind, r.revision_id, r.content_hash,
                       r.nonce, r.ciphertext, r.created_at_ms
                FROM namespace_resources r
                JOIN namespaces n ON n.namespace_id = r.namespace_id
                LEFT JOIN namespace_resource_heads h
                    ON h.namespace_id = r.namespace_id AND h.name = r.name
                WHERE n.name = ?1 AND r.name = ?2
                  AND r.resource_id = COALESCE(?3, h.resource_id)
                ",
                params![namespace, name, resource_id],
                |row| {
                    Ok(StoredNamespaceResource {
                        metadata: NamespaceResource {
                            resource_id: row.get(0)?,
                            namespace: namespace.to_string(),
                            name: name.to_string(),
                            kind: row.get(1)?,
                            revision_id: row.get(2)?,
                            content_hash: row.get(3)?,
                            created_at_ms: row.get(6)?,
                        },
                        nonce: row.get(4)?,
                        ciphertext: row.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub fn list_namespace_resources(
        &self,
        namespace: &str,
    ) -> Result<Vec<NamespaceResource>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT r.resource_id, r.name, r.kind, r.revision_id,
                   r.content_hash, r.created_at_ms
            FROM namespace_resource_heads h
            JOIN namespaces n ON n.namespace_id = h.namespace_id
            JOIN namespace_resources r ON r.resource_id = h.resource_id
            WHERE n.name = ?1
            ORDER BY r.name
            ",
        )?;
        let rows = stmt.query_map(params![namespace], |row| {
            Ok(NamespaceResource {
                resource_id: row.get(0)?,
                namespace: namespace.to_string(),
                name: row.get(1)?,
                kind: row.get(2)?,
                revision_id: row.get(3)?,
                content_hash: row.get(4)?,
                created_at_ms: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn create_snapshot(
        &mut self,
        namespace: &str,
        generation: Option<i64>,
    ) -> Result<i64, rusqlite::Error> {
        let tx = self.conn.transaction()?;
        let namespace_id = Self::ensure_namespace_tx(&tx, namespace)?;
        let generation = match generation {
            Some(generation) => generation,
            None => Self::latest_generation_tx(&tx, namespace_id)?.unwrap_or(0),
        };

        tx.execute(
            "INSERT INTO snapshots (namespace_id, generation) VALUES (?1, ?2)",
            params![namespace_id, generation],
        )?;
        let snapshot_id = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare(
                "
                SELECT symbol, arity, artifact_hash, revision_id
                FROM bindings
                WHERE namespace_id = ?1 AND generation = ?2 AND active = 1
                ",
            )?;
            let rows = stmt.query_map(params![namespace_id, generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;

            for row in rows {
                let (symbol, arity, artifact_hash, revision_id) = row?;
                tx.execute(
                    "
                    INSERT INTO snapshot_bindings (
                        snapshot_id, symbol, arity, artifact_hash, revision_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5)
                    ",
                    params![snapshot_id, symbol, arity, artifact_hash, revision_id],
                )?;
            }
        }

        tx.commit()?;
        Ok(snapshot_id)
    }

    pub fn resolve_snapshot_bindings(
        &self,
        snapshot_id: i64,
    ) -> Result<HashMap<(String, usize), String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT symbol, arity, artifact_hash, revision_id
            FROM snapshot_bindings
            WHERE snapshot_id = ?1
            ",
        )?;
        let rows = stmt.query_map(params![snapshot_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut resolved = HashMap::new();
        for row in rows {
            let (symbol, arity, artifact_hash) = row?;
            resolved.insert((symbol, arity as usize), artifact_hash);
        }
        Ok(resolved)
    }

    pub fn list_namespaces(&self) -> Result<Vec<NamespaceSummary>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT n.name,
                   (SELECT MAX(g.generation) FROM namespace_generations g WHERE g.namespace_id = n.namespace_id),
                   (SELECT COUNT(*) FROM snapshots s WHERE s.namespace_id = n.namespace_id),
                   (SELECT COUNT(*) FROM bindings b
                    WHERE b.namespace_id = n.namespace_id AND b.active = 1
                      AND b.generation = (SELECT MAX(g.generation) FROM namespace_generations g
                                          WHERE g.namespace_id = n.namespace_id))
            FROM namespaces n
            ORDER BY n.name
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(NamespaceSummary {
                name: row.get(0)?,
                current_generation: row.get(1)?,
                snapshot_count: row.get(2)?,
                binding_count: row.get(3)?,
            })
        })?;
        let mut namespaces = Vec::new();
        for row in rows {
            namespaces.push(row?);
        }
        Ok(namespaces)
    }

    pub fn get_namespace_bindings(
        &self,
        namespace: &str,
        generation: Option<i64>,
    ) -> Result<Option<(i64, Vec<PublishedBinding>)>, rusqlite::Error> {
        let namespace_id = self
            .conn
            .query_row(
                "SELECT namespace_id FROM namespaces WHERE name = ?1",
                params![namespace],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(namespace_id) = namespace_id else {
            return Ok(None);
        };
        let generation = match generation {
            Some(generation) => generation,
            None => self
                .conn
                .query_row(
                    "SELECT MAX(generation) FROM namespace_generations WHERE namespace_id = ?1",
                    params![namespace_id],
                    |row| row.get::<_, Option<i64>>(0),
                )?
                .unwrap_or(0),
        };

        let mut stmt = self.conn.prepare(
            "
            SELECT symbol, arity, artifact_hash, generation, revision_id
            FROM bindings
            WHERE namespace_id = ?1 AND generation = ?2 AND active = 1
            ORDER BY symbol, arity
            ",
        )?;
        let rows = stmt.query_map(params![namespace_id, generation], |row| {
            Ok(PublishedBinding {
                symbol: row.get(0)?,
                arity: row.get::<_, i64>(1)? as usize,
                artifact_hash: row.get(2)?,
                generation: row.get(3)?,
                revision_id: row.get(4)?,
            })
        })?;
        let mut bindings = Vec::new();
        for row in rows {
            bindings.push(row?);
        }
        Ok(Some((generation, bindings)))
    }

    pub fn resolve_snapshot_binding(
        &self,
        snapshot_id: i64,
        symbol: &str,
    ) -> Result<Option<(String, usize, Option<String>)>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT artifact_hash, arity, revision_id
                FROM snapshot_bindings
                WHERE snapshot_id = ?1 AND symbol = ?2
                ",
                params![snapshot_id, symbol],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as usize, row.get(2)?)),
            )
            .optional()
    }

    pub fn get_snapshot_summary(
        &self,
        snapshot_id: i64,
    ) -> Result<Option<SnapshotSummary>, rusqlite::Error> {
        let snapshot = self
            .conn
            .query_row(
                "
                SELECT n.name, s.generation
                FROM snapshots s
                JOIN namespaces n ON n.namespace_id = s.namespace_id
                WHERE s.snapshot_id = ?1
                ",
                params![snapshot_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((namespace, generation)) = snapshot else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "
            SELECT symbol, arity, artifact_hash, revision_id
            FROM snapshot_bindings
            WHERE snapshot_id = ?1
            ORDER BY symbol, arity
            ",
        )?;
        let rows = stmt.query_map(params![snapshot_id], |row| {
            Ok(PublishedBinding {
                symbol: row.get(0)?,
                arity: row.get::<_, i64>(1)? as usize,
                artifact_hash: row.get(2)?,
                generation,
                revision_id: row.get(3)?,
            })
        })?;
        let mut bindings = Vec::new();
        for row in rows {
            bindings.push(row?);
        }
        Ok(Some(SnapshotSummary {
            snapshot_id,
            namespace,
            generation,
            bindings,
        }))
    }

    pub fn diff_namespace_generations(
        &self,
        namespace: &str,
        from_generation: i64,
        to_generation: i64,
    ) -> Result<Option<NamespaceDiff>, rusqlite::Error> {
        let Some((_, from_bindings)) =
            self.get_namespace_bindings(namespace, Some(from_generation))?
        else {
            return Ok(None);
        };
        let Some((_, to_bindings)) = self.get_namespace_bindings(namespace, Some(to_generation))?
        else {
            return Ok(None);
        };

        let mut from_map = HashMap::new();
        for binding in from_bindings {
            from_map.insert(
                binding.symbol.clone(),
                (binding.arity, binding.artifact_hash),
            );
        }
        let mut to_map = HashMap::new();
        for binding in to_bindings {
            to_map.insert(
                binding.symbol.clone(),
                (binding.arity, binding.artifact_hash),
            );
        }

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();

        for (symbol, (arity, to_artifact_hash)) in &to_map {
            match from_map.get(symbol) {
                None => added.push(BindingChange {
                    symbol: symbol.clone(),
                    arity: *arity,
                    from_artifact_hash: None,
                    to_artifact_hash: Some(to_artifact_hash.clone()),
                }),
                Some((from_arity, from_artifact_hash))
                    if from_arity != arity || from_artifact_hash != to_artifact_hash =>
                {
                    changed.push(BindingChange {
                        symbol: symbol.clone(),
                        arity: *arity,
                        from_artifact_hash: Some(from_artifact_hash.clone()),
                        to_artifact_hash: Some(to_artifact_hash.clone()),
                    });
                }
                _ => {}
            }
        }

        for (symbol, (arity, from_artifact_hash)) in &from_map {
            if !to_map.contains_key(symbol) {
                removed.push(BindingChange {
                    symbol: symbol.clone(),
                    arity: *arity,
                    from_artifact_hash: Some(from_artifact_hash.clone()),
                    to_artifact_hash: None,
                });
            }
        }

        added.sort_by(|left, right| left.symbol.cmp(&right.symbol));
        removed.sort_by(|left, right| left.symbol.cmp(&right.symbol));
        changed.sort_by(|left, right| left.symbol.cmp(&right.symbol));

        Ok(Some(NamespaceDiff {
            namespace: namespace.to_string(),
            from_generation,
            to_generation,
            added,
            removed,
            changed,
        }))
    }

    pub fn get_artifact(
        &self,
        artifact_hash: &str,
    ) -> Result<Option<FunctionArtifact>, rusqlite::Error> {
        let artifact = self
            .conn
            .query_row(
                "
                SELECT source_name, body_hash, abi_hash, arity, build_key, core_source
                FROM artifacts
                WHERE artifact_hash = ?1
                ",
                params![artifact_hash],
                |row| {
                    Ok(FunctionArtifact {
                        source_name: row.get(0)?,
                        body_hash: row.get(1)?,
                        abi_hash: row.get(2)?,
                        artifact_hash: artifact_hash.to_string(),
                        arity: row.get::<_, i64>(3)? as usize,
                        build_key: row.get(4)?,
                        core_source: row.get(5)?,
                        dependencies: Vec::new(),
                    })
                },
            )
            .optional()?;

        artifact
            .map(|mut artifact| {
                artifact.dependencies = self.get_artifact_dependencies(artifact_hash)?;
                Ok(artifact)
            })
            .transpose()
    }

    pub fn get_artifact_dependencies(
        &self,
        artifact_hash: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT callee_hash
            FROM artifact_deps
            WHERE artifact_hash = ?1
            ORDER BY callee_hash
            ",
        )?;
        let rows = stmt.query_map(params![artifact_hash], |row| row.get::<_, String>(0))?;

        let mut deps = Vec::new();
        for row in rows {
            deps.push(row?);
        }
        Ok(deps)
    }

    pub fn insert_execution(&self, record: &ExecutionRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "
            INSERT INTO executions (
                execution_id, request_kind, snapshot_id, entry_artifact_hash, target_selector,
                source_hash, status, error_message, started_at_ms, finished_at_ms, wall_time_ms,
                stdout_bytes, stderr_bytes, stdout_preview, stderr_preview, arg_fingerprint
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ",
            params![
                record.execution_id,
                record.request_kind,
                record.snapshot_id,
                record.entry_artifact_hash,
                record.target_selector,
                record.source_hash,
                record.status,
                record.error_message,
                record.started_at_ms,
                record.finished_at_ms,
                record.wall_time_ms,
                record.stdout_bytes,
                record.stderr_bytes,
                record.stdout_preview,
                record.stderr_preview,
                record.arg_fingerprint,
            ],
        )?;
        Ok(())
    }

    pub fn insert_deployment(&self, deployment: &DeploymentRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "
            INSERT INTO deployments (
                deployment_id, snapshot_id, entry_artifact_hash, target_selector, args_json, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                deployment.deployment_id,
                deployment.snapshot_id,
                deployment.entry_artifact_hash,
                deployment.target_selector,
                deployment.args_json,
                deployment.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn insert_instance(&self, instance: &InstanceRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "
            INSERT INTO instances (
                instance_id, deployment_id, snapshot_id, entry_artifact_hash, target_selector,
                args_json, status, started_at_ms, stopped_at_ms, pid, exit_code, run_dir
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            params![
                instance.instance_id,
                instance.deployment_id,
                instance.snapshot_id,
                instance.entry_artifact_hash,
                instance.target_selector,
                instance.args_json,
                instance.status,
                instance.started_at_ms,
                instance.stopped_at_ms,
                instance.pid.map(i64::from),
                instance.exit_code.map(i64::from),
                instance.run_dir,
            ],
        )?;
        Ok(())
    }

    pub fn update_instance_state(
        &self,
        instance_id: &str,
        status: &str,
        stopped_at_ms: Option<i64>,
        exit_code: Option<i32>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "
            UPDATE instances
            SET status = ?2, stopped_at_ms = ?3, exit_code = ?4
            WHERE instance_id = ?1
            ",
            params![instance_id, status, stopped_at_ms, exit_code.map(i64::from),],
        )?;
        Ok(())
    }

    pub fn get_instance(
        &self,
        instance_id: &str,
    ) -> Result<Option<InstanceRecord>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT deployment_id, snapshot_id, entry_artifact_hash, target_selector, args_json,
                       status, started_at_ms, stopped_at_ms, pid, exit_code, run_dir
                FROM instances
                WHERE instance_id = ?1
                ",
                params![instance_id],
                |row| {
                    Ok(InstanceRecord {
                        instance_id: instance_id.to_string(),
                        deployment_id: row.get(0)?,
                        snapshot_id: row.get(1)?,
                        entry_artifact_hash: row.get(2)?,
                        target_selector: row.get(3)?,
                        args_json: row.get(4)?,
                        status: row.get(5)?,
                        started_at_ms: row.get(6)?,
                        stopped_at_ms: row.get(7)?,
                        pid: row.get::<_, Option<i64>>(8)?.map(|value| value as u32),
                        exit_code: row.get::<_, Option<i64>>(9)?.map(|value| value as i32),
                        run_dir: row.get(10)?,
                    })
                },
            )
            .optional()
    }

    pub fn list_instances(&self) -> Result<Vec<InstanceRecord>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "
            SELECT instance_id
            FROM instances
            ORDER BY started_at_ms DESC
            ",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut instances = Vec::new();
        for row in rows {
            let instance_id = row?;
            if let Some(instance) = self.get_instance(&instance_id)? {
                instances.push(instance);
            }
        }
        Ok(instances)
    }

    pub fn get_execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<ExecutionRecord>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT request_kind, snapshot_id, entry_artifact_hash, target_selector,
                       source_hash, status, error_message, started_at_ms, finished_at_ms,
                       wall_time_ms, stdout_bytes, stderr_bytes, stdout_preview,
                       stderr_preview, arg_fingerprint
                FROM executions
                WHERE execution_id = ?1
                ",
                params![execution_id],
                |row| {
                    Ok(ExecutionRecord {
                        execution_id: execution_id.to_string(),
                        request_kind: row.get(0)?,
                        snapshot_id: row.get(1)?,
                        entry_artifact_hash: row.get(2)?,
                        target_selector: row.get(3)?,
                        source_hash: row.get(4)?,
                        status: row.get(5)?,
                        error_message: row.get(6)?,
                        started_at_ms: row.get(7)?,
                        finished_at_ms: row.get(8)?,
                        wall_time_ms: row.get(9)?,
                        stdout_bytes: row.get(10)?,
                        stderr_bytes: row.get(11)?,
                        stdout_preview: row.get(12)?,
                        stderr_preview: row.get(13)?,
                        arg_fingerprint: row.get(14)?,
                    })
                },
            )
            .optional()
    }

    pub fn latest_execution(&self) -> Result<Option<ExecutionRecord>, rusqlite::Error> {
        let execution_id = self
            .conn
            .query_row(
                "SELECT execution_id FROM executions ORDER BY started_at_ms DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match execution_id {
            Some(execution_id) => self.get_execution(&execution_id),
            None => Ok(None),
        }
    }

    pub fn list_recent_executions(
        &self,
        limit: usize,
    ) -> Result<Vec<ExecutionRecord>, rusqlite::Error> {
        self.list_executions(limit, None, None, None, None)
    }

    pub fn list_executions(
        &self,
        limit: usize,
        before_started_at_ms: Option<i64>,
        snapshot_id: Option<i64>,
        request_kind: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<ExecutionRecord>, rusqlite::Error> {
        let mut query = String::from(
            "
            SELECT execution_id, request_kind, snapshot_id, entry_artifact_hash, target_selector,
                   source_hash, status, error_message, started_at_ms, finished_at_ms,
                   wall_time_ms, stdout_bytes, stderr_bytes, stdout_preview, stderr_preview,
                   arg_fingerprint
            FROM executions
            WHERE 1 = 1
            ",
        );
        let mut params = Vec::new();

        if let Some(before_started_at_ms) = before_started_at_ms {
            query.push_str(" AND started_at_ms < ?");
            params.push(SqlValue::Integer(before_started_at_ms));
        }
        if let Some(snapshot_id) = snapshot_id {
            query.push_str(" AND snapshot_id = ?");
            params.push(SqlValue::Integer(snapshot_id));
        }
        if let Some(request_kind) = request_kind {
            query.push_str(" AND request_kind = ?");
            params.push(SqlValue::Text(request_kind.to_string()));
        }
        if let Some(status) = status {
            query.push_str(" AND status = ?");
            params.push(SqlValue::Text(status.to_string()));
        }

        query.push_str(" ORDER BY started_at_ms DESC, rowid DESC LIMIT ?");
        params.push(SqlValue::Integer(limit as i64));

        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt.query_map(params_from_iter(params), |row| {
            Ok(ExecutionRecord {
                execution_id: row.get(0)?,
                request_kind: row.get(1)?,
                snapshot_id: row.get(2)?,
                entry_artifact_hash: row.get(3)?,
                target_selector: row.get(4)?,
                source_hash: row.get(5)?,
                status: row.get(6)?,
                error_message: row.get(7)?,
                started_at_ms: row.get(8)?,
                finished_at_ms: row.get(9)?,
                wall_time_ms: row.get(10)?,
                stdout_bytes: row.get(11)?,
                stderr_bytes: row.get(12)?,
                stdout_preview: row.get(13)?,
                stderr_preview: row.get(14)?,
                arg_fingerprint: row.get(15)?,
            })
        })?;

        let mut executions = Vec::new();
        for row in rows {
            executions.push(row?);
        }
        Ok(executions)
    }

    pub fn prune_executions(&self, finished_before_ms: i64) -> Result<usize, rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM executions WHERE finished_at_ms < ?1",
            params![finished_before_ms],
        )
    }

    pub fn execution_count(&self) -> Result<i64, rusqlite::Error> {
        self.conn
            .query_row("SELECT COUNT(*) FROM executions", [], |row| row.get(0))
    }

    fn insert_artifact_tx(
        tx: &rusqlite::Transaction<'_>,
        source_module: &str,
        artifact: &FunctionArtifact,
    ) -> Result<(), rusqlite::Error> {
        tx.execute(
            "
            INSERT OR REPLACE INTO artifacts (
                artifact_hash, body_hash, abi_hash, source_name, arity, build_key, core_source, source_module
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                artifact.artifact_hash,
                artifact.body_hash,
                artifact.abi_hash,
                artifact.source_name,
                artifact.arity as i64,
                artifact.build_key,
                artifact.core_source,
                source_module
            ],
        )?;

        tx.execute(
            "DELETE FROM artifact_deps WHERE artifact_hash = ?1",
            params![artifact.artifact_hash],
        )?;
        for dep in &artifact.dependencies {
            tx.execute(
                "
                INSERT OR REPLACE INTO artifact_deps (artifact_hash, callee_hash, call_kind)
                VALUES (?1, ?2, 'apply')
                ",
                params![artifact.artifact_hash, dep],
            )?;
        }
        Ok(())
    }

    fn ensure_namespace_tx(
        tx: &rusqlite::Transaction<'_>,
        namespace: &str,
    ) -> Result<i64, rusqlite::Error> {
        tx.execute(
            "INSERT OR IGNORE INTO namespaces (name) VALUES (?1)",
            params![namespace],
        )?;
        tx.query_row(
            "SELECT namespace_id FROM namespaces WHERE name = ?1",
            params![namespace],
            |row| row.get(0),
        )
    }

    fn latest_generation_tx(
        tx: &rusqlite::Transaction<'_>,
        namespace_id: i64,
    ) -> Result<Option<i64>, rusqlite::Error> {
        tx.query_row(
            "SELECT MAX(generation) FROM namespace_generations WHERE namespace_id = ?1",
            params![namespace_id],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.flatten())
    }

    pub fn current_namespace_generation(
        &self,
        namespace: &str,
    ) -> Result<Option<i64>, rusqlite::Error> {
        self.conn
            .query_row(
                "
                SELECT MAX(g.generation)
                FROM namespaces n
                LEFT JOIN namespace_generations g ON g.namespace_id = n.namespace_id
                WHERE n.name = ?1
                ",
                params![namespace],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(|value| value.flatten())
    }

    fn next_generation_tx(
        tx: &rusqlite::Transaction<'_>,
        namespace_id: i64,
    ) -> Result<i64, rusqlite::Error> {
        Ok(Self::latest_generation_tx(tx, namespace_id)?.unwrap_or(0) + 1)
    }

    fn ensure_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), rusqlite::Error> {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        match self.conn.execute(&sql, []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_then_snapshot_resolves_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let generation = store
            .publish_bindings(
                "dev",
                &[NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v1".to_string(),
                }],
            )
            .unwrap();
        let snapshot = store.create_snapshot("dev", Some(generation)).unwrap();
        let bindings = store.resolve_snapshot_bindings(snapshot).unwrap();

        assert_eq!(
            bindings.get(&("main".to_string(), 0)),
            Some(&"hash_main_v1".to_string())
        );
    }

    #[test]
    fn insert_package_round_trips_artifact_and_dependencies() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let package = CompiledPackage {
            source_module: "test".to_string(),
            entry_module: Some("hash_main".to_string()),
            entry_arity: Some(0),
            artifacts: vec![FunctionArtifact {
                source_name: "main".to_string(),
                body_hash: "body_main".to_string(),
                abi_hash: "abi_main".to_string(),
                artifact_hash: "hash_main".to_string(),
                build_key: "build_main".to_string(),
                arity: 0,
                core_source: "module 'hash_main'".to_string(),
                dependencies: vec!["hash_dep".to_string()],
            }],
        };

        store.insert_package(&package).unwrap();
        let artifact = store.get_artifact("hash_main").unwrap().unwrap();

        assert_eq!(artifact.source_name, "main");
        assert_eq!(artifact.abi_hash, "abi_main");
        assert_eq!(artifact.build_key, "build_main");
        assert_eq!(artifact.dependencies, vec!["hash_dep".to_string()]);
    }

    #[test]
    fn insert_execution_round_trips_summary() {
        let store = SqliteStore::open_in_memory().unwrap();
        let record = ExecutionRecord {
            execution_id: "exec_1".to_string(),
            request_kind: "run".to_string(),
            snapshot_id: 7,
            entry_artifact_hash: Some("hash_main".to_string()),
            target_selector: Some("main/0".to_string()),
            source_hash: None,
            status: "success".to_string(),
            error_message: None,
            started_at_ms: 100,
            finished_at_ms: 125,
            wall_time_ms: 25,
            stdout_bytes: 2,
            stderr_bytes: 0,
            stdout_preview: "55".to_string(),
            stderr_preview: String::new(),
            arg_fingerprint: "args_hash".to_string(),
        };

        store.insert_execution(&record).unwrap();
        let round_trip = store.get_execution("exec_1").unwrap().unwrap();

        assert_eq!(round_trip.request_kind, "run");
        assert_eq!(round_trip.entry_artifact_hash.as_deref(), Some("hash_main"));
        assert_eq!(round_trip.stdout_preview, "55");
        assert_eq!(round_trip.wall_time_ms, 25);
    }

    #[test]
    fn namespace_and_snapshot_queries_return_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let generation = store
            .publish_bindings(
                "dev",
                &[NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v1".to_string(),
                }],
            )
            .unwrap();
        let snapshot_id = store.create_snapshot("dev", Some(generation)).unwrap();

        let namespaces = store.list_namespaces().unwrap();
        assert_eq!(namespaces.len(), 1);
        assert_eq!(namespaces[0].name, "dev");
        assert_eq!(namespaces[0].current_generation, Some(1));

        let namespace = store.get_namespace_bindings("dev", None).unwrap().unwrap();
        assert_eq!(namespace.0, 1);
        assert_eq!(namespace.1[0].artifact_hash, "hash_main_v1");

        let snapshot = store.get_snapshot_summary(snapshot_id).unwrap().unwrap();
        assert_eq!(snapshot.namespace, "dev");
        assert_eq!(snapshot.bindings[0].symbol, "main");
    }

    #[test]
    fn namespace_diff_reports_added_and_changed_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .publish_bindings(
                "dev",
                &[NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v1".to_string(),
                }],
            )
            .unwrap();
        store
            .publish_bindings(
                "dev",
                &[
                    NamespaceBinding {
                        symbol: "main".to_string(),
                        arity: 0,
                        artifact_hash: "hash_main_v2".to_string(),
                    },
                    NamespaceBinding {
                        symbol: "helper".to_string(),
                        arity: 0,
                        artifact_hash: "hash_helper_v1".to_string(),
                    },
                ],
            )
            .unwrap();

        let diff = store
            .diff_namespace_generations("dev", 1, 2)
            .unwrap()
            .unwrap();
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].symbol, "helper");
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].symbol, "main");
        assert_eq!(
            diff.changed[0].from_artifact_hash.as_deref(),
            Some("hash_main_v1")
        );
        assert_eq!(
            diff.changed[0].to_artifact_hash.as_deref(),
            Some("hash_main_v2")
        );
    }

    #[test]
    fn namespace_generations_carry_forward_unchanged_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .publish_bindings(
                "dev",
                &[
                    NamespaceBinding {
                        symbol: "main".to_string(),
                        arity: 0,
                        artifact_hash: "hash_main_v1".to_string(),
                    },
                    NamespaceBinding {
                        symbol: "helper".to_string(),
                        arity: 0,
                        artifact_hash: "hash_helper_v1".to_string(),
                    },
                ],
            )
            .unwrap();
        let generation = store
            .publish_bindings(
                "dev",
                &[NamespaceBinding {
                    symbol: "main".to_string(),
                    arity: 0,
                    artifact_hash: "hash_main_v2".to_string(),
                }],
            )
            .unwrap();

        let (_, bindings) = store
            .get_namespace_bindings("dev", Some(generation))
            .unwrap()
            .unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].symbol, "helper");
        assert_eq!(bindings[0].artifact_hash, "hash_helper_v1");
        assert_eq!(bindings[1].symbol, "main");
        assert_eq!(bindings[1].artifact_hash, "hash_main_v2");
    }

    #[test]
    fn namespace_changeset_revisions_round_trip_without_file_identity() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let revision = NamespaceRevision {
            revision_id: "revision_1".to_string(),
            namespace: "protocol".to_string(),
            changeset: "packet-builder".to_string(),
            parent_revision_id: None,
            base_snapshot_id: None,
            source_hash: "source_hash_1".to_string(),
            source: "fn build_packet() { 42 }".to_string(),
            sandbox: true,
            capabilities: vec!["net.udp".to_string()],
            created_at_ms: 100,
        };

        store.insert_changeset_revision(&revision).unwrap();

        let summary = store
            .get_changeset("protocol", "packet-builder")
            .unwrap()
            .unwrap();
        assert_eq!(summary.head_revision_id.as_deref(), Some("revision_1"));
        assert_eq!(summary.revision_count, 1);
        let round_trip = store
            .get_changeset_revision("protocol", "packet-builder", None)
            .unwrap()
            .unwrap();
        assert_eq!(round_trip.source, revision.source);
        assert_eq!(round_trip.source_hash, "source_hash_1");
        assert!(round_trip.sandbox);
        assert_eq!(round_trip.capabilities, vec!["net.udp"]);
    }

    #[test]
    fn instance_records_round_trip_and_update_state() {
        let store = SqliteStore::open_in_memory().unwrap();
        let deployment = DeploymentRecord {
            deployment_id: "dep_1".to_string(),
            snapshot_id: 3,
            entry_artifact_hash: "hash_main".to_string(),
            target_selector: "main/0".to_string(),
            args_json: "[]".to_string(),
            created_at_ms: 100,
        };
        let instance = InstanceRecord {
            instance_id: "inst_1".to_string(),
            deployment_id: "dep_1".to_string(),
            snapshot_id: 3,
            entry_artifact_hash: "hash_main".to_string(),
            target_selector: "main/0".to_string(),
            args_json: "[]".to_string(),
            status: "running".to_string(),
            started_at_ms: 101,
            stopped_at_ms: None,
            pid: Some(1234),
            exit_code: None,
            run_dir: "/tmp/inst_1".to_string(),
        };

        store.insert_deployment(&deployment).unwrap();
        store.insert_instance(&instance).unwrap();
        store
            .update_instance_state("inst_1", "stopped", Some(110), Some(0))
            .unwrap();

        let round_trip = store.get_instance("inst_1").unwrap().unwrap();
        assert_eq!(round_trip.status, "stopped");
        assert_eq!(round_trip.stopped_at_ms, Some(110));
        assert_eq!(round_trip.exit_code, Some(0));
    }
}
