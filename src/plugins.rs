use std::{
    ffi::CString,
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::Semaphore;
use wasmi::{Config, EnforcedLimits, Engine, Linker, Module, Store, StoreLimitsBuilder};

use crate::{auth::AdminUser, db, error::ApiError, state::AppState};

const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_INPUT_BYTES: usize = 8 * 1024;
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
const MODULE_FUEL: u64 = 10_000_000;
const MODULE_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_OFFSET: usize = 1024;
const OUTPUT_OFFSET: usize = 16 * 1024;
const OUTPUT_CAPACITY: usize = 48 * 1024;
const PLUGIN_REGISTRY_LOCK_KEY: i64 = 82_473_014;

static PLUGIN_WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn worker_slots() -> Arc<Semaphore> {
    PLUGIN_WORKERS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
}

pub(crate) fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
}

pub fn router(state: AppState) -> Router<()> {
    Router::new()
        .route("/Puffinbox/Plugins", get(list_plugins))
        .route("/Puffinbox/Plugins/TrustStaged", post(trust_staged_plugin))
        .route("/Puffinbox/Plugins/{plugin_id}/Enable", post(enable_plugin))
        .route(
            "/Puffinbox/Plugins/{plugin_id}/Disable",
            post(disable_plugin),
        )
        .with_state(state)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PluginManifest {
    id: String,
    name: String,
    version: String,
    api_version: u16,
    hook: String,
    module: String,
    module_sha256: String,
    license: String,
    provenance: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PluginIdRequest {
    plugin_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PluginDto {
    plugin_id: String,
    name: String,
    version: String,
    api_version: i16,
    declared_license: String,
    declared_provenance: String,
    enabled: bool,
    status: String,
    last_error_code: Option<String>,
    installed_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

async fn list_plugins(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> Result<Json<Vec<PluginDto>>, ApiError> {
    let rows = sqlx::query("SELECT plugin_id,name,version,api_version,declared_license,declared_provenance,enabled,status,last_error_code,installed_at,updated_at FROM trusted_plugins ORDER BY plugin_id LIMIT 256")
        .fetch_all(&state.db)
        .await?;
    let mut plugins = Vec::with_capacity(rows.len());
    for row in rows {
        plugins.push(PluginDto {
            plugin_id: row.try_get("plugin_id")?,
            name: row.try_get("name")?,
            version: row.try_get("version")?,
            api_version: row.try_get("api_version")?,
            declared_license: row.try_get("declared_license")?,
            declared_provenance: row.try_get("declared_provenance")?,
            enabled: row.try_get("enabled")?,
            status: row.try_get("status")?,
            last_error_code: row.try_get("last_error_code")?,
            installed_at: row.try_get("installed_at")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(Json(plugins))
}

async fn trust_staged_plugin(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Json(request): Json<PluginIdRequest>,
) -> Result<(StatusCode, Json<PluginDto>), ApiError> {
    if !valid_plugin_id(&request.plugin_id) {
        return Err(ApiError::BadRequest(
            "PluginId must match the bounded lowercase ID format".to_owned(),
        ));
    }
    let data_dir = state.config.data_dir.clone();
    let plugin_id = request.plugin_id.clone();
    let staged = run_blocking_bounded(move || read_staged_plugin(&data_dir, &plugin_id)).await??;
    validate_manifest(&staged.manifest, &request.plugin_id)?;
    let module_bytes = staged.module_bytes.clone();
    let validation_input = plugin_validation_input();
    run_blocking_bounded(move || {
        validate_and_execute(&module_bytes, &validation_input)
            .map(|_| ())
            .map_err(|code| {
                ApiError::BadRequest(format!("Staged plugin validation failed: {code}"))
            })
    })
    .await??;

    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    lock_plugin_registry(&mut tx).await?;
    let existing: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM trusted_plugins WHERE plugin_id=$1)")
            .bind(&staged.manifest.id)
            .fetch_one(&mut *tx)
            .await?;
    let installed_count: i64 = sqlx::query_scalar("SELECT count(*) FROM trusted_plugins")
        .fetch_one(&mut *tx)
        .await?;
    if !existing && installed_count >= 256 {
        return Err(ApiError::Conflict(
            "The server is limited to 256 trusted plugin manifests".to_owned(),
        ));
    }
    sqlx::query("INSERT INTO trusted_plugins(plugin_id,name,version,api_version,manifest_sha256,binary_sha256,declared_license,declared_provenance,enabled,status,last_error_code) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,FALSE,'disabled',NULL) ON CONFLICT(plugin_id) DO UPDATE SET name=EXCLUDED.name,version=EXCLUDED.version,api_version=EXCLUDED.api_version,manifest_sha256=EXCLUDED.manifest_sha256,binary_sha256=EXCLUDED.binary_sha256,declared_license=EXCLUDED.declared_license,declared_provenance=EXCLUDED.declared_provenance,enabled=FALSE,status='disabled',last_error_code=NULL,installed_at=NOW(),updated_at=NOW()")
        .bind(&staged.manifest.id)
        .bind(&staged.manifest.name)
        .bind(&staged.manifest.version)
        .bind(i16::try_from(staged.manifest.api_version).map_err(|_| ApiError::BadRequest("Unsupported plugin API version".to_owned()))?)
        .bind(&staged.manifest_sha256)
        .bind(&staged.module_sha256)
        .bind(&staged.manifest.license)
        .bind(&staged.manifest.provenance)
        .execute(&mut *tx)
        .await?;
    clear_plugin_metadata(&mut tx, &staged.manifest.id).await?;
    tx.commit().await?;
    let plugin = fetch_plugin(&state.db, &request.plugin_id).await?;
    Ok((StatusCode::CREATED, Json(plugin)))
}

async fn enable_plugin(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    AxumPath(plugin_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if !valid_plugin_id(&plugin_id) {
        return Err(ApiError::NotFound);
    }
    let data_dir = state.config.data_dir.clone();
    let id = plugin_id.clone();
    let staged = run_blocking_bounded(move || read_staged_plugin(&data_dir, &id)).await??;
    validate_manifest(&staged.manifest, &plugin_id)?;
    let module = staged.module_bytes.clone();
    let validation_input = plugin_validation_input();
    run_blocking_bounded(move || {
        validate_and_execute(&module, &validation_input)
            .map(|_| ())
            .map_err(|code| {
                ApiError::BadRequest(format!("Staged plugin validation failed: {code}"))
            })
    })
    .await??;

    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    lock_plugin_registry(&mut tx).await?;
    let stored = sqlx::query(
        "SELECT manifest_sha256,binary_sha256 FROM trusted_plugins WHERE plugin_id=$1 FOR UPDATE",
    )
    .bind(&plugin_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let manifest_hash: String = stored.try_get("manifest_sha256")?;
    let binary_hash: String = stored.try_get("binary_sha256")?;
    if !plugin_hashes_match(
        &manifest_hash,
        &binary_hash,
        &staged.manifest_sha256,
        &staged.module_sha256,
    ) {
        return Err(ApiError::Conflict("Staged plugin files differ from the operator-trusted hashes; trust them again before enabling".to_owned()));
    }
    let enabled_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM trusted_plugins WHERE enabled=TRUE AND plugin_id<>$1",
    )
    .bind(&plugin_id)
    .fetch_one(&mut *tx)
    .await?;
    if enabled_count >= 32 {
        return Err(ApiError::Conflict(
            "The server is limited to 32 enabled metadata hooks".to_owned(),
        ));
    }
    sqlx::query("UPDATE trusted_plugins SET enabled=TRUE,status='enabled',last_error_code=NULL,updated_at=NOW() WHERE plugin_id=$1")
        .bind(&plugin_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn disable_plugin(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    AxumPath(plugin_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if !valid_plugin_id(&plugin_id) {
        return Err(ApiError::NotFound);
    }
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    lock_plugin_registry(&mut tx).await?;
    let result = sqlx::query("UPDATE trusted_plugins SET enabled=FALSE,status='disabled',updated_at=NOW() WHERE plugin_id=$1")
        .bind(&plugin_id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    clear_plugin_metadata(&mut tx, &plugin_id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_plugin_metadata(
    tx: &mut Transaction<'_, Postgres>,
    plugin_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM item_metadata WHERE provider_key=$1")
        .bind(format!("plugin:{plugin_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn fetch_plugin(pool: &PgPool, id: &str) -> Result<PluginDto, ApiError> {
    let row = sqlx::query("SELECT plugin_id,name,version,api_version,declared_license,declared_provenance,enabled,status,last_error_code,installed_at,updated_at FROM trusted_plugins WHERE plugin_id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(PluginDto {
        plugin_id: row.try_get("plugin_id")?,
        name: row.try_get("name")?,
        version: row.try_get("version")?,
        api_version: row.try_get("api_version")?,
        declared_license: row.try_get("declared_license")?,
        declared_provenance: row.try_get("declared_provenance")?,
        enabled: row.try_get("enabled")?,
        status: row.try_get("status")?,
        last_error_code: row.try_get("last_error_code")?,
        installed_at: row.try_get("installed_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn lock_plugin_registry(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PLUGIN_REGISTRY_LOCK_KEY)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

struct StagedPlugin {
    manifest: PluginManifest,
    manifest_sha256: String,
    module_sha256: String,
    module_bytes: Vec<u8>,
}

fn read_staged_plugin(data_dir: &Path, plugin_id: &str) -> Result<StagedPlugin, ApiError> {
    let root = std::fs::canonicalize(data_dir).map_err(|_| ApiError::NotFound)?;
    let root_dir = open_directory(&root).map_err(|_| ApiError::NotFound)?;
    let plugins_dir = open_child_directory(&root_dir, "plugins").map_err(|_| ApiError::NotFound)?;
    let plugin_dir =
        open_child_directory(&plugins_dir, plugin_id).map_err(|_| ApiError::NotFound)?;
    let manifest_bytes = read_child_bounded(&plugin_dir, "manifest.json", MAX_MANIFEST_BYTES)
        .map_err(|_| {
            ApiError::BadRequest("Could not read the staged plugin manifest".to_owned())
        })?;
    let manifest: PluginManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
        ApiError::BadRequest("The staged plugin manifest is invalid JSON".to_owned())
    })?;
    validate_manifest(&manifest, plugin_id)?;
    let module_bytes = read_child_bounded(&plugin_dir, &manifest.module, MAX_MODULE_BYTES)
        .map_err(|_| ApiError::BadRequest("Could not read the staged plugin module".to_owned()))?;
    let module_sha256 = sha256_hex(&module_bytes);
    if module_sha256 != manifest.module_sha256 {
        return Err(ApiError::Conflict(
            "The staged module hash does not match its manifest".to_owned(),
        ));
    }
    Ok(StagedPlugin {
        manifest,
        manifest_sha256: sha256_hex(&manifest_bytes),
        module_sha256,
        module_bytes,
    })
}

fn validate_manifest(manifest: &PluginManifest, expected_id: &str) -> Result<(), ApiError> {
    if manifest.id != expected_id
        || !valid_plugin_id(&manifest.id)
        || manifest.name.trim().is_empty()
        || manifest.name.len() > 128
        || manifest.version.trim().is_empty()
        || manifest.version.len() > 64
        || manifest.api_version != 1
        || manifest.hook != "metadata.enrich.v1"
        || !matches!(
            manifest.module.as_str(),
            "module.wasm" | "module.wat" | "metadata-enricher.wat"
        )
        || !is_sha256(&manifest.module_sha256)
        || manifest.license.trim().is_empty()
        || manifest.license.len() > 128
        || manifest.provenance.trim().is_empty()
        || manifest.provenance.len() > 1024
        || manifest.license.chars().any(char::is_control)
        || manifest.provenance.chars().any(char::is_control)
    {
        return Err(ApiError::BadRequest(
            "The staged plugin manifest violates the supported v1 format".to_owned(),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(input: &[u8]) -> String {
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(target_os = "linux")]
fn open_directory(path: &Path) -> std::io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: the CString remains alive for the call; the returned descriptor
    // is immediately converted to an owned File.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was returned as a new owned descriptor by `open`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_child_directory(parent: &File, name: &str) -> std::io::Result<File> {
    let name =
        CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: the parent is an owned directory fd and the name contains no NUL.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was returned as a new owned descriptor by `openat`.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn read_child_bounded(parent: &File, name: &str, limit: usize) -> std::io::Result<Vec<u8>> {
    let name =
        CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: the parent is an owned directory fd and the name contains no NUL.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was returned as a new owned descriptor by `openat`.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    Ok(bytes)
}

#[cfg(not(target_os = "linux"))]
fn open_directory(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(not(target_os = "linux"))]
fn open_child_directory(_parent: &File, _name: &str) -> std::io::Result<File> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(not(target_os = "linux"))]
fn read_child_bounded(_parent: &File, _name: &str, _limit: usize) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

async fn run_blocking_bounded<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<Result<T, ApiError>, ApiError> {
    let permit = tokio::time::timeout(MODULE_TIMEOUT, worker_slots().acquire_owned())
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    });
    tokio::time::timeout(MODULE_TIMEOUT, task)
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Internal("bounded plugin worker stopped unexpectedly".to_owned()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PluginInput {
    pub id: String,
    pub name: String,
    pub item_type: String,
    pub overview: Option<String>,
}

fn plugin_validation_input() -> Vec<u8> {
    serde_json::to_vec(&PluginInput {
        id: "00000000-0000-4000-8000-000000000001".to_owned(),
        name: "Validation input".to_owned(),
        item_type: "Movie".to_owned(),
        overview: None,
    })
    .expect("the fixed plugin validation input serializes")
}

fn plugin_hashes_match(
    expected_manifest: &str,
    expected_module: &str,
    staged_manifest: &str,
    staged_module: &str,
) -> bool {
    expected_manifest.trim() == staged_manifest && expected_module.trim() == staged_module
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PluginOutput {
    pub overview: Option<String>,
    pub genres: Option<Vec<String>>,
}

pub(crate) struct PluginExecution {
    pub output: PluginOutput,
    pub manifest_sha256: String,
    pub binary_sha256: String,
}

pub(crate) async fn run_enabled_plugin(
    state: &AppState,
    plugin_id: &str,
    input: PluginInput,
) -> Result<PluginExecution, &'static str> {
    if !valid_plugin_id(plugin_id) {
        return Err("plugin-invalid-id");
    }
    let row = sqlx::query("SELECT manifest_sha256,binary_sha256 FROM trusted_plugins WHERE plugin_id=$1 AND enabled=TRUE AND status='enabled'")
        .bind(plugin_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| "plugin-database-error")?
        .ok_or("plugin-not-enabled")?;
    let expected_manifest: String = row
        .try_get("manifest_sha256")
        .map_err(|_| "plugin-database-error")?;
    let expected_module: String = row
        .try_get("binary_sha256")
        .map_err(|_| "plugin-database-error")?;
    let data_dir = state.config.data_dir.clone();
    let id = plugin_id.to_owned();
    let staged = run_blocking_bounded(move || read_staged_plugin(&data_dir, &id))
        .await
        .map_err(|_| "plugin-worker-busy")?
        .map_err(|_| "plugin-stage-invalid")?;
    if !plugin_hashes_match(
        &expected_manifest,
        &expected_module,
        &staged.manifest_sha256,
        &staged.module_sha256,
    ) {
        return Err("plugin-hash-changed");
    }
    let module = staged.module_bytes;
    let input = serde_json::to_vec(&input).map_err(|_| "plugin-input-invalid")?;
    if input.len() > MAX_INPUT_BYTES {
        return Err("plugin-input-too-large");
    }
    let output = run_blocking_bounded(move || {
        validate_and_execute(&module, &input)
            .map_err(|_| ApiError::BadRequest("plugin-execution-failed".to_owned()))
    })
    .await
    .map_err(|_| "plugin-worker-timeout")?
    .map_err(|_| "plugin-execution-failed")?;
    Ok(PluginExecution {
        output,
        manifest_sha256: staged.manifest_sha256,
        binary_sha256: staged.module_sha256,
    })
}

pub(crate) async fn verify_enabled_plugin_tx(
    tx: &mut Transaction<'_, Postgres>,
    plugin_id: &str,
    expected_hashes: Option<(&str, &str)>,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT enabled,status,manifest_sha256,binary_sha256 FROM trusted_plugins WHERE plugin_id=$1 FOR SHARE")
        .bind(plugin_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let enabled: bool = row.try_get("enabled")?;
    let status: String = row.try_get("status")?;
    let stored_manifest: String = row.try_get("manifest_sha256")?;
    let stored_binary: String = row.try_get("binary_sha256")?;
    Ok(enabled
        && status == "enabled"
        && expected_hashes.is_none_or(|(manifest, binary)| {
            stored_manifest.trim() == manifest && stored_binary.trim() == binary
        }))
}

fn validate_and_execute(module_bytes: &[u8], input: &[u8]) -> Result<PluginOutput, String> {
    if module_bytes.is_empty()
        || module_bytes.len() > MAX_MODULE_BYTES
        || input.len() > MAX_INPUT_BYTES
    {
        return Err("module-or-input-size".to_owned());
    }
    let mut config = Config::default();
    config
        .set_max_recursion_depth(64)
        .set_max_stack_height(1024)
        .consume_fuel(true)
        .ignore_custom_sections(true)
        .enforced_limits(EnforcedLimits::strict());
    let engine = Engine::new(&config);
    let module = Module::new(&engine, module_bytes).map_err(|_| "module-invalid")?;
    if module.imports().next().is_some() {
        return Err("plugin-imports-not-allowed".to_owned());
    }
    let limits = StoreLimitsBuilder::new()
        .memory_size(1024 * 1024)
        .table_elements(128)
        .instances(1)
        .tables(1)
        .memories(1)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|limits| limits);
    store
        .set_fuel(MODULE_FUEL)
        .map_err(|_| "fuel-unavailable")?;
    let linker = Linker::<wasmi::StoreLimits>::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(|_| "instantiate-or-start-failed")?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or_else(|| "memory-export-missing".to_owned())?;
    let enrich = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&store, "enrich")
        .map_err(|_| "hook-export-signature")?;
    let input_ptr = i32::try_from(INPUT_OFFSET).map_err(|_| "input-offset-overflow")?;
    let input_len = i32::try_from(input.len()).map_err(|_| "input-length-overflow")?;
    let output_ptr = i32::try_from(OUTPUT_OFFSET).map_err(|_| "output-offset-overflow")?;
    let output_capacity = i32::try_from(OUTPUT_CAPACITY).map_err(|_| "output-capacity-overflow")?;
    memory
        .write(&mut store, INPUT_OFFSET, input)
        .map_err(|_| "input-memory-write")?;
    let output_len = enrich
        .call(
            &mut store,
            (input_ptr, input_len, output_ptr, output_capacity),
        )
        .map_err(|_| "hook-trapped")?;
    if output_len <= 0
        || output_len as usize > MAX_OUTPUT_BYTES
        || output_len as usize > OUTPUT_CAPACITY
    {
        return Err("hook-output-length".to_owned());
    }
    let mut output = vec![0_u8; output_len as usize];
    memory
        .read(&store, OUTPUT_OFFSET, &mut output)
        .map_err(|_| "output-memory-read")?;
    let parsed: PluginOutput =
        serde_json::from_slice(&output).map_err(|_| "hook-output-invalid")?;
    validate_plugin_output(parsed)
}

fn validate_plugin_output(output: PluginOutput) -> Result<PluginOutput, String> {
    if output
        .overview
        .as_ref()
        .is_some_and(|value| value.len() > 20_000)
        || output.genres.as_ref().is_some_and(|genres| {
            genres.len() > 128 || genres.iter().any(|genre| genre.len() > 128)
        })
    {
        return Err("hook-output-limit".to_owned());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        MAX_INPUT_BYTES, PluginInput, is_sha256, plugin_hashes_match, plugin_validation_input,
        read_staged_plugin, sha256_hex, valid_plugin_id, validate_and_execute,
    };

    #[test]
    fn plugin_ids_match_the_database_grammar() {
        for id in ["a", "7plugin", "metadata-enricher", "a.b_c-9"] {
            assert!(valid_plugin_id(id), "rejected valid plugin id {id}");
        }
        for id in ["", "APlugin", "../x", "a/b", "plugin:"] {
            assert!(!valid_plugin_id(id), "accepted invalid plugin id {id}");
        }
        assert!(!valid_plugin_id(&"a".repeat(65)));
    }

    #[test]
    fn sha256_declarations_are_lowercase_hex_only() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"A".repeat(64)));
        assert!(!is_sha256(&"a".repeat(63)));
    }

    #[test]
    fn original_example_module_runs_without_host_imports() {
        let bytes = include_bytes!("../examples/plugins/metadata-enricher.wat");
        let input = serde_json::to_vec(&PluginInput {
            id: "item-1".to_owned(),
            name: "Example".to_owned(),
            item_type: "Movie".to_owned(),
            overview: None,
        })
        .unwrap();
        let output = validate_and_execute(bytes, &input).unwrap();
        assert_eq!(
            output.overview.as_deref(),
            Some("Applied the original example metadata hook.")
        );
    }

    #[test]
    fn staged_validation_input_uses_the_production_item_type_field() {
        let bytes = plugin_validation_input();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["id"], "00000000-0000-4000-8000-000000000001");
        assert!(uuid::Uuid::parse_str(payload["id"].as_str().unwrap()).is_ok());
        assert_eq!(payload["name"], "Validation input");
        assert_eq!(payload["itemType"], "Movie");
        assert_eq!(payload["overview"], serde_json::Value::Null);
        assert!(payload.get("type").is_none());
        assert_eq!(
            bytes,
            serde_json::to_vec(&PluginInput {
                id: "00000000-0000-4000-8000-000000000001".to_owned(),
                name: "Validation input".to_owned(),
                item_type: "Movie".to_owned(),
                overview: None,
            })
            .unwrap()
        );
    }

    #[test]
    fn rejects_unsupported_host_imports() {
        let imported = br#"(module (import "host" "clock" (func)))"#;
        assert_eq!(
            validate_and_execute(imported, b"{}").unwrap_err(),
            "plugin-imports-not-allowed"
        );
    }

    #[test]
    fn hostile_modules_cannot_grow_past_the_memory_limit() {
        let module = br#"(module
            (memory (export "memory") 1 32)
            (func (export "enrich") (param i32 i32 i32 i32) (result i32)
                i32.const 16
                memory.grow
                i32.const -1
                i32.eq
                if
                    unreachable
                end
                i32.const 2))"#;
        assert_eq!(
            validate_and_execute(module, b"{}").unwrap_err(),
            "hook-trapped"
        );
    }

    #[test]
    fn hostile_modules_cannot_exhaust_unbounded_execution_fuel() {
        let module = br#"(module
            (memory (export "memory") 1 16)
            (func (export "enrich") (param i32 i32 i32 i32) (result i32)
                (loop $forever
                    br $forever)
                unreachable))"#;
        assert_eq!(
            validate_and_execute(module, b"{}").unwrap_err(),
            "hook-trapped"
        );
    }

    #[test]
    fn plugin_input_and_output_lengths_are_bounded_before_copying() {
        let oversized_input = vec![b'x'; MAX_INPUT_BYTES + 1];
        assert_eq!(
            validate_and_execute(b"not wasm", &oversized_input).unwrap_err(),
            "module-or-input-size"
        );

        let oversized_output = br#"(module
            (memory (export "memory") 1 16)
            (func (export "enrich") (param i32 i32 i32 i32) (result i32)
                i32.const 49153))"#;
        assert_eq!(
            validate_and_execute(oversized_output, b"{}").unwrap_err(),
            "hook-output-length"
        );

        let oversized = super::PluginOutput {
            overview: Some("x".repeat(20_001)),
            genres: None,
        };
        assert!(super::validate_plugin_output(oversized).is_err());
    }

    #[test]
    fn trusted_hash_check_rejects_manifest_or_module_changes() {
        let manifest_hash = "a".repeat(64);
        let module_hash = "b".repeat(64);
        assert!(plugin_hashes_match(
            &format!("{manifest_hash}   "),
            &module_hash,
            &manifest_hash,
            &module_hash,
        ));
        assert!(!plugin_hashes_match(
            &"c".repeat(64),
            &module_hash,
            &manifest_hash,
            &module_hash,
        ));
        assert!(!plugin_hashes_match(
            &manifest_hash,
            &"d".repeat(64),
            &manifest_hash,
            &module_hash,
        ));
    }

    #[test]
    fn staged_module_hash_change_is_rejected() {
        let data_dir = std::env::temp_dir().join(format!(
            "puffinbox-plugin-hash-test-{}",
            uuid::Uuid::new_v4()
        ));
        let plugin_dir = data_dir.join("plugins").join("hash-fixture");
        fs::create_dir_all(&plugin_dir).unwrap();
        let module_bytes = include_bytes!("../examples/plugins/metadata-enricher.wat");
        let manifest = serde_json::json!({
            "id": "hash-fixture",
            "name": "Hash fixture",
            "version": "1.0.0",
            "apiVersion": 1,
            "hook": "metadata.enrich.v1",
            "module": "metadata-enricher.wat",
            "moduleSha256": sha256_hex(module_bytes),
            "license": "MIT OR Apache-2.0",
            "provenance": "Original test fixture"
        });
        fs::write(
            plugin_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let module_path = plugin_dir.join("metadata-enricher.wat");
        fs::write(&module_path, module_bytes).unwrap();

        let staged = read_staged_plugin(&data_dir, "hash-fixture").unwrap();
        assert_eq!(staged.module_sha256, sha256_hex(module_bytes));
        fs::write(&module_path, b"(module)").unwrap();
        assert!(matches!(
            read_staged_plugin(&data_dir, "hash-fixture"),
            Err(crate::ApiError::Conflict(_))
        ));
        fs::remove_dir_all(data_dir).unwrap();
    }
}
