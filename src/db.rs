use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder, Row, types::Json};
use uuid::Uuid;

mod catalog_filters;
pub(crate) use catalog_filters::{CatalogFacets, GenreFacet, catalog_facets};
mod catalog_relations;
pub(crate) use catalog_relations::similar_items;
mod music_credits;
mod music_mix;
mod music_tag_artists;
mod next_up;
pub(crate) use next_up::{NextUpQuery, next_up_items};
mod studios;
pub(crate) use music_credits::{MusicArtistCredit, MusicArtistRole, music_artist_page};
pub(crate) use music_mix::{MusicMixSeed, instant_mix, visible_music_genre};
pub(crate) use music_tag_artists::register_metadata_artists;
pub(crate) use studios::{StudioRecord, StudioSelection, set_studio_favorite, studio_page};

use crate::{
    auth::UserRecord,
    library::{ItemQuery, ItemRecord, LibraryRecord},
};

const UNRATED_CATEGORY_SQL: &str = "(CASE WHEN i.metadata_json->>'LiveTvRecording'='true' THEN 'LiveTvProgram' WHEN i.item_type IN ('Folder','CollectionFolder','Season','BoxSet','Series','MusicArtist','MusicAlbum') THEN NULL WHEN i.item_type='Movie' THEN 'Movie' WHEN i.item_type='Trailer' THEN 'Trailer' WHEN i.item_type='Episode' THEN 'Series' WHEN i.item_type IN ('Audio','MusicVideo') THEN 'Music' WHEN i.item_type IN ('Book','AudioBook','EBook') THEN 'Book' WHEN i.item_type='LiveTvChannel' THEN 'LiveTvChannel' WHEN i.item_type='LiveTvProgram' THEN 'LiveTvProgram' WHEN i.item_type='ChannelContent' THEN 'ChannelContent' ELSE 'Other' END)";
const FOLDER_ITEM_TYPES_SQL: &str =
    "('Folder','CollectionFolder','Season','BoxSet','Series','MusicArtist','MusicAlbum')";
const VISIBLE_LIBRARY_MEDIA_BASE_SQL: &str = "i.item_type NOT IN ('Folder','CollectionFolder','Season','BoxSet','Series','MusicArtist','MusicAlbum') AND i.item_type <> 'File' AND i.path !~ '(^|/)[.]'";
const LIVE_TV_CHANNEL_ENABLED_SQL: &str = "(i.item_type <> 'LiveTvChannel' OR EXISTS (SELECT 1 FROM live_tv_channels tv JOIN live_tv_sources src ON src.id=tv.source_id AND src.library_id=tv.library_id WHERE tv.item_id=i.id AND tv.enabled=TRUE AND src.enabled=TRUE))";
const LOGIN_THROTTLE_CAPACITY_LOCK: i64 = 82_473_013;
const LOGIN_THROTTLE_BUCKET_LIMIT: i64 = 100_000;
const LOGIN_THROTTLE_DATA_BUCKET_LIMIT: i64 = LOGIN_THROTTLE_BUCKET_LIMIT - 1;
const LOGIN_THROTTLE_CAPACITY_BUCKET: &str =
    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
pub const SERVER_INSTANCE_LOCK_KEY: i64 = 82_473_012;
pub const STALE_SERVER_RUN_ERROR: &str = "stale server run";

pub async fn try_server_instance_lock(pool: &PgPool) -> Result<Option<PgConnection>, sqlx::Error> {
    let mut connection = pool.acquire().await?.detach();
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SERVER_INSTANCE_LOCK_KEY)
        .fetch_one(&mut connection)
        .await?;
    // Detach before attempting the lock so cancellation during this query also
    // closes its backend instead of returning a possibly locked session to the
    // pool. The owner remains dedicated until explicitly unlocked or dropped.
    Ok(acquired.then_some(connection))
}

pub(crate) fn is_folder_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "Folder"
            | "CollectionFolder"
            | "Season"
            | "BoxSet"
            | "Series"
            | "MusicArtist"
            | "MusicAlbum"
    )
}

fn item_category(item_type: &str) -> Option<&'static str> {
    match item_type {
        "Folder" | "CollectionFolder" | "Season" | "BoxSet" | "Series" | "MusicArtist"
        | "MusicAlbum" => None,
        "Movie" => Some("Movie"),
        "Trailer" => Some("Trailer"),
        "Episode" => Some("Series"),
        "Audio" | "MusicVideo" => Some("Music"),
        "Book" | "AudioBook" | "EBook" => Some("Book"),
        "LiveTvChannel" => Some("LiveTvChannel"),
        "LiveTvProgram" => Some("LiveTvProgram"),
        "ChannelContent" => Some("ChannelContent"),
        _ => Some("Other"),
    }
}

#[derive(Clone, Debug)]
pub struct NewUser {
    pub username: String,
    pub password_hash: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub enable_remote_access: bool,
    pub allow_media_playback: bool,
    pub enable_content_downloading: bool,
    pub enable_live_tv_access: bool,
    pub enable_live_tv_management: bool,
    pub max_parental_rating: Option<i32>,
    pub block_unrated_items: Vec<String>,
    /// None means all enabled libraries; Some(empty) means no libraries.
    pub allowed_library_ids: Option<Vec<Uuid>>,
}

#[derive(Clone, Debug, Default)]
pub struct UserPatch {
    pub username: Option<String>,
    pub password_hash: Option<String>,
    pub is_admin: Option<bool>,
    pub disabled: Option<bool>,
    pub enable_remote_access: Option<bool>,
    pub allow_media_playback: Option<bool>,
    pub enable_content_downloading: Option<bool>,
    pub enable_live_tv_access: Option<bool>,
    pub enable_live_tv_management: Option<bool>,
    pub max_parental_rating: Option<Option<i32>>,
    pub block_unrated_items: Option<Vec<String>>,
    pub allowed_library_ids: Option<Vec<Uuid>>,
    pub enable_all_folders: Option<bool>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct UserItemData {
    pub played: bool,
    pub play_count: i32,
    pub is_favorite: bool,
    pub playback_position_ticks: i64,
    pub last_played_at: Option<DateTime<Utc>>,
    pub rating: Option<f64>,
}

#[derive(Default)]
pub(crate) struct UserItemDataPatch {
    pub played: Option<bool>,
    pub favorite: Option<bool>,
    pub position_ticks: Option<i64>,
    pub play_count: Option<i32>,
    pub last_played_at: Option<DateTime<Utc>>,
    pub rating: Option<f64>,
}

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct ItemCounts {
    pub movie_count: i64,
    pub series_count: i64,
    pub episode_count: i64,
    pub artist_count: i64,
    pub album_count: i64,
    pub song_count: i64,
    pub book_count: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ItemNavigationLinks {
    pub series_id: Option<Uuid>,
    pub series_name: Option<String>,
    pub season_id: Option<Uuid>,
    pub season_name: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub album_id: Option<Uuid>,
    pub album: Option<String>,
    pub artist_id: Option<Uuid>,
    pub artist: Option<String>,
    pub music: Option<music_credits::MusicNavigation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryRootIdentity {
    pub library_id: Uuid,
    pub library_name: String,
    pub root_path: String,
    pub device_id: String,
    pub inode: String,
    pub last_verified_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRebindResult {
    Rebound,
    AlreadyCurrent,
    LibraryMissing,
    RootNotConfigured,
    IdentityMissing,
    IdentityChanged,
    ScanRunning,
    StaleRun,
}

#[derive(Clone, Debug)]
pub struct RootRebindRequest {
    pub library_id: Uuid,
    pub root_path: PathBuf,
    pub expected_device_id: String,
    pub expected_inode: String,
    pub current_device_id: String,
    pub current_inode: String,
}

#[derive(Clone, Debug)]
pub struct AuthSessionRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub client: String,
    pub device_name: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub capabilities: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct PlaybackSessionRecord {
    pub id: Uuid,
    pub item_id: Option<Uuid>,
    pub position_ticks: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStartResult {
    Started,
    AlreadyActive,
    Conflict,
    StaleRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryScanClaim {
    Claimed,
    AlreadyRunning,
    LibraryMissing,
    StaleRun,
}

#[derive(Clone, Debug)]
pub struct NewAuthToken {
    pub token_id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub client: String,
    pub device_name: String,
    pub device_id: String,
}

#[derive(Clone, Debug)]
pub struct PlaybackStartRequest {
    pub id: Uuid,
    pub run_id: Uuid,
    pub user_id: Uuid,
    pub item_id: Uuid,
    pub device_id: String,
    pub device_name: String,
    pub client: String,
    pub play_method: Option<String>,
    pub position_ticks: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct PlaybackSessionSelector {
    pub run_id: Uuid,
    pub user_id: Uuid,
    pub device_id: String,
    pub id: Option<Uuid>,
    pub item_id: Option<Uuid>,
}

pub fn path_hash(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub struct VirtualItemInput<'a> {
    pub library_id: Uuid,
    pub item_id: Uuid,
    pub name: &'a str,
    pub item_type: &'a str,
    pub opaque_path: &'a str,
    pub overview: Option<&'a str>,
    pub metadata_json: &'a Value,
}

/// Upsert the catalog identity required by a Live TV channel row. The channel
/// path is an opaque protocol identifier and is never a filesystem path.
/// Callers must insert or update the corresponding `live_tv_channels` row in
/// the same transaction after this function succeeds.
pub async fn upsert_virtual_item(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    item: VirtualItemInput<'_>,
) -> Result<(), sqlx::Error> {
    require_active_run(tx, run_id).await?;
    let VirtualItemInput {
        library_id,
        item_id,
        name,
        item_type,
        opaque_path,
        overview,
        metadata_json,
    } = item;
    if item_type != "LiveTvChannel"
        || name.trim().is_empty()
        || name.len() > 512
        || opaque_path.len() > 2048
        || !opaque_path.starts_with("puffinbox://livetv/")
        || !opaque_path.is_ascii()
        || opaque_path.contains('\0')
        || overview.is_some_and(|value| value.len() > 20_000)
        || !metadata_json.is_object()
    {
        return Err(sqlx::Error::Protocol(
            "invalid Live TV virtual item fields".to_owned(),
        ));
    }
    let sort_name = name.to_lowercase();
    let hashed_path = path_hash(opaque_path);
    let upserted = sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,container,size_bytes,runtime_ticks,rating,overview,metadata_json,last_seen_scan) VALUES ($1,$2,NULL,$3,$4,'LiveTvChannel',$5,$6,NULL,NULL,NULL,NULL,$7,$8,NULL) ON CONFLICT (id) DO UPDATE SET name=EXCLUDED.name,sort_name=EXCLUDED.sort_name,overview=EXCLUDED.overview,metadata_json=EXCLUDED.metadata_json,date_modified=NOW() WHERE items.library_id=EXCLUDED.library_id AND items.path=EXCLUDED.path AND items.path_hash=EXCLUDED.path_hash AND items.item_type='LiveTvChannel' RETURNING id")
        .bind(item_id)
        .bind(library_id)
        .bind(name)
        .bind(sort_name)
        .bind(opaque_path)
        .bind(hashed_path)
        .bind(overview)
        .bind(Json(metadata_json.clone()))
        .fetch_optional(&mut **tx)
        .await?;
    if upserted.is_none() {
        return Err(sqlx::Error::Protocol(
            "Live TV virtual item identity or path collision".to_owned(),
        ));
    }
    Ok(())
}

pub async fn persisted_server_id(pool: &PgPool, run_id: Uuid) -> Result<Uuid, sqlx::Error> {
    let candidate = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("INSERT INTO instance_meta(key, value) VALUES ('server_id', $1) ON CONFLICT (key) DO NOTHING")
        .bind(&candidate)
        .execute(&mut *tx)
        .await?;
    let raw: String = sqlx::query_scalar("SELECT value FROM instance_meta WHERE key = 'server_id'")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Uuid::parse_str(&raw)
        .map_err(|e| sqlx::Error::Protocol(format!("persisted server_id is invalid: {e}")))
}

/// Set the active run marker before migrations (or fence a run before
/// releasing ownership during shutdown). On first startup the metadata table
/// does not exist yet, so the initial migrations remain the only safe next step.
pub async fn set_active_run_marker(pool: &PgPool, run_id: Uuid) -> Result<bool, sqlx::Error> {
    let metadata_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('instance_meta') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !metadata_exists {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO instance_meta(key,value) VALUES ('active_run_id',$1) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value",
    )
    .bind(run_id.to_string())
    .execute(pool)
    .await?;
    Ok(true)
}

pub async fn set_active_run_marker_on_connection(
    connection: &mut PgConnection,
    run_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let metadata_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('instance_meta') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !metadata_exists {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO instance_meta(key,value) VALUES ('active_run_id',$1) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value",
    )
    .bind(run_id.to_string())
    .execute(&mut *connection)
    .await?;
    Ok(true)
}

/// Atomically fence this server run before it begins serving requests. Updating
/// the marker takes an exclusive row lock, which waits for older playback
/// mutations holding a shared lock and prevents them from writing afterward.
pub async fn activate_run(pool: &PgPool, run_id: Uuid) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instance_meta(key, value) VALUES ('active_run_id', $1) ON CONFLICT (key) DO NOTHING",
    )
    .bind(run_id.to_string())
    .execute(&mut *tx)
    .await?;
    let active_run_id: String =
        sqlx::query_scalar("SELECT value FROM instance_meta WHERE key='active_run_id' FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    if active_run_id != run_id.to_string() {
        tx.rollback().await?;
        return Err(sqlx::Error::Protocol(
            "this server run was fenced before startup completed".into(),
        ));
    }
    let ended = sqlx::query(
        "UPDATE playback_sessions SET ended_at=NOW(),last_activity_at=NOW() WHERE ended_at IS NULL AND instance_run_id<>$1",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let interrupted = sqlx::query(
        "UPDATE library_scan_state SET status='interrupted',finished_at=NOW(),last_error='Server restarted before the scan completed',updated_at=NOW() WHERE status='running'",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let _requeued_metadata_jobs = sqlx::query(
        "UPDATE metadata_refresh_runs SET status='queued',claimed_run_id=NULL,started_at=NULL,finished_at=NULL,next_attempt_at=NULL,last_error_code='server-restarted',updated_at=NOW() WHERE status='running' AND claimed_run_id IS DISTINCT FROM $1",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let _requeued_offline_jobs = sqlx::query(
        "UPDATE offline_packages SET status='queued',claimed_run_id=NULL,started_at=NULL,bytes_copied=0,error_code='server-restarted',updated_at=NOW() WHERE status='running' AND claimed_run_id IS DISTINCT FROM $1",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((ended, interrupted))
}

pub(crate) async fn active_run_is_current(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let active: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_meta WHERE key='active_run_id' FOR SHARE")
            .fetch_optional(&mut **tx)
            .await?;
    let expected = run_id.to_string();
    Ok(active.as_deref() == Some(expected.as_str()))
}

pub(crate) async fn require_active_run(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
) -> Result<(), sqlx::Error> {
    if active_run_is_current(tx, run_id).await? {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol(format!(
            "{STALE_SERVER_RUN_ERROR}: database mutation is fenced"
        )))
    }
}

pub async fn claim_library_scan(
    pool: &PgPool,
    run_id: Uuid,
    library_id: Uuid,
    scan_id: Uuid,
) -> Result<LibraryScanClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(LibraryScanClaim::StaleRun);
    }
    let library_exists =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM libraries WHERE id=$1 FOR UPDATE")
            .bind(library_id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
    if !library_exists {
        tx.rollback().await?;
        return Ok(LibraryScanClaim::LibraryMissing);
    }
    let claimed = sqlx::query("INSERT INTO library_scan_state(library_id,scan_id,status,files_seen,directories_seen,items_indexed,errors,skipped_entries,started_at,finished_at,last_error,updated_at) VALUES ($1,$2,'running',0,0,0,0,0,NOW(),NULL,NULL,NOW()) ON CONFLICT(library_id) DO UPDATE SET scan_id=EXCLUDED.scan_id,status='running',files_seen=0,directories_seen=0,items_indexed=0,errors=0,skipped_entries=0,started_at=NOW(),finished_at=NULL,last_error=NULL,updated_at=NOW() WHERE library_scan_state.status <> 'running' RETURNING scan_id")
        .bind(library_id)
        .bind(scan_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    tx.commit().await?;
    Ok(if claimed {
        LibraryScanClaim::Claimed
    } else {
        LibraryScanClaim::AlreadyRunning
    })
}

pub async fn user_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM users")
        .fetch_one(pool)
        .await
}

pub async fn get_user(pool: &PgPool, id: Uuid) -> Result<Option<UserRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT u.id, u.username, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id = u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM users u WHERE u.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(user_from_row).transpose()
}

pub async fn find_user_by_name(
    pool: &PgPool,
    username: &str,
) -> Result<Option<(UserRecord, String)>, sqlx::Error> {
    let normalized = username.trim().to_ascii_lowercase();
    let row = sqlx::query(
        "SELECT u.id, u.username, u.password_hash, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id = u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM users u WHERE u.username_norm = $1",
    )
    .bind(normalized)
    .fetch_optional(pool)
    .await?;
    row.as_ref()
        .map(|row| {
            let user = user_from_row(row)?;
            let hash: String = row.try_get("password_hash")?;
            Ok((user, hash))
        })
        .transpose()
}

pub async fn list_users(pool: &PgPool) -> Result<Vec<UserRecord>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id = u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM users u ORDER BY u.username_norm",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(user_from_row).collect()
}

pub async fn create_user(
    pool: &PgPool,
    run_id: Uuid,
    input: &NewUser,
) -> Result<UserRecord, sqlx::Error> {
    let username = input.username.trim().to_owned();
    let normalized = username.to_ascii_lowercase();
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    // Serialize every new user with source refreshes that lock the current
    // user set before replacing guide rows. Otherwise a concurrent non-admin
    // insert could be absent from that row-lock set and immediately create a
    // timer while the refresh holds program/FK locks.
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *tx)
        .await?;
    if let Some(ids) = &input.allowed_library_ids {
        validate_library_ids(&mut tx, ids).await?;
    }
    let restrict_libraries = input.allowed_library_ids.is_some() && !input.is_admin;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id, username, username_norm, password_hash, is_admin, disabled, enable_remote_access, allow_media_playback, restrict_libraries, max_parental_rating, block_unrated_items, enable_content_downloading, enable_live_tv_access, enable_live_tv_management) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
        .bind(id)
        .bind(&username)
        .bind(&normalized)
        .bind(&input.password_hash)
        .bind(input.is_admin)
        .bind(input.disabled)
        .bind(input.enable_remote_access)
        .bind(input.allow_media_playback)
        .bind(restrict_libraries)
        .bind(input.max_parental_rating)
        .bind(input.block_unrated_items.clone())
        .bind(input.enable_content_downloading)
        .bind(input.enable_live_tv_access)
        .bind(input.enable_live_tv_management)
        .execute(&mut *tx)
        .await?;
    if restrict_libraries {
        for library_id in input.allowed_library_ids.as_ref().expect("checked Some") {
            sqlx::query("INSERT INTO user_library_access(user_id, library_id) VALUES ($1,$2)")
                .bind(id)
                .bind(library_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    get_user(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound)
}

pub async fn bootstrap_first_admin(
    pool: &PgPool,
    run_id: Uuid,
    username: &str,
    password_hash: &str,
) -> Result<Option<UserRecord>, sqlx::Error> {
    let name = username.trim().to_owned();
    let normalized = name.to_ascii_lowercase();
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM users")
        .fetch_one(&mut *tx)
        .await?;
    if count > 0 {
        return Ok(None);
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,allow_media_playback,restrict_libraries,enable_live_tv_access,enable_live_tv_management) VALUES ($1,$2,$3,$4,TRUE,TRUE,TRUE,FALSE,TRUE,TRUE)")
        .bind(id).bind(&name).bind(normalized).bind(password_hash).execute(&mut *tx).await?;
    tx.commit().await?;
    get_user(pool, id).await
}

pub async fn update_user(
    pool: &PgPool,
    run_id: Uuid,
    id: Uuid,
    patch: &UserPatch,
) -> Result<Option<UserRecord>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT u.id, u.username, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id = u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM users u WHERE u.id=$1 FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?;
    let Some(existing) = row.as_ref().map(user_from_row).transpose()? else {
        return Ok(None);
    };
    let username = patch
        .username
        .as_deref()
        .map(str::trim)
        .unwrap_or(&existing.username)
        .to_owned();
    let normalized = username.to_ascii_lowercase();
    let next_is_admin = patch.is_admin.unwrap_or(existing.is_admin);
    let next_disabled = patch.disabled.unwrap_or(existing.disabled);
    if existing.is_admin && !existing.disabled && !(next_is_admin && !next_disabled) {
        let admins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM users WHERE is_admin=TRUE AND disabled=FALSE",
        )
        .fetch_one(&mut *tx)
        .await?;
        if admins <= 1 {
            return Err(sqlx::Error::Protocol(
                "cannot remove the last enabled administrator".to_owned(),
            ));
        }
    }
    if let Some(ids) = &patch.allowed_library_ids {
        validate_library_ids(&mut tx, ids).await?;
        sqlx::query("DELETE FROM user_library_access WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for library_id in ids {
            sqlx::query("INSERT INTO user_library_access(user_id, library_id) VALUES ($1,$2)")
                .bind(id)
                .bind(library_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let restrict_libraries = if next_is_admin {
        false
    } else if let Some(enable_all) = patch.enable_all_folders {
        !enable_all
    } else if patch.allowed_library_ids.is_some() {
        true
    } else {
        existing.restrict_libraries
    };
    if patch.enable_all_folders == Some(true) {
        sqlx::query("DELETE FROM user_library_access WHERE user_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE users SET username=$2, username_norm=$3, password_hash=COALESCE($4,password_hash), is_admin=COALESCE($5,is_admin), disabled=COALESCE($6,disabled), enable_remote_access=COALESCE($7,enable_remote_access), allow_media_playback=COALESCE($8,allow_media_playback), restrict_libraries=$9, max_parental_rating=CASE WHEN $10 THEN $11::SMALLINT ELSE max_parental_rating END, block_unrated_items=COALESCE($12,block_unrated_items), enable_content_downloading=COALESCE($13,enable_content_downloading), enable_live_tv_access=COALESCE($14,enable_live_tv_access), enable_live_tv_management=COALESCE($15,enable_live_tv_management), updated_at=NOW() WHERE id=$1")
        .bind(id)
        .bind(username)
        .bind(normalized)
        .bind(&patch.password_hash)
        .bind(patch.is_admin)
        .bind(patch.disabled)
        .bind(patch.enable_remote_access)
        .bind(patch.allow_media_playback)
        .bind(restrict_libraries)
        .bind(patch.max_parental_rating.is_some())
        .bind(patch.max_parental_rating.flatten().map(|value| value as i16))
        .bind(patch.block_unrated_items.clone())
        .bind(patch.enable_content_downloading)
        .bind(patch.enable_live_tv_access)
        .bind(patch.enable_live_tv_management)
        .execute(&mut *tx)
        .await?;
    if patch.password_hash.is_some() {
        sqlx::query(
            "UPDATE auth_tokens SET revoked_at=NOW() WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    get_user(pool, id).await
}

pub async fn delete_user(pool: &PgPool, run_id: Uuid, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT is_admin, disabled FROM users WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let is_enabled_admin = row.as_ref().is_some_and(|row| {
        row.try_get::<bool, _>("is_admin").unwrap_or(false)
            && !row.try_get::<bool, _>("disabled").unwrap_or(true)
    });
    if is_enabled_admin {
        let admins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM users WHERE is_admin=TRUE AND disabled=FALSE",
        )
        .fetch_one(&mut *tx)
        .await?;
        if admins <= 1 {
            return Err(sqlx::Error::Protocol(
                "cannot delete the last enabled administrator".to_owned(),
            ));
        }
    }
    let result = sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

async fn validate_library_ids(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM libraries WHERE id = ANY($1)")
            .bind(ids)
            .fetch_one(&mut **tx)
            .await?;
    if count
        != ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len() as i64
    {
        return Err(sqlx::Error::Protocol(
            "one or more library IDs do not exist".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn user_from_row(row: &sqlx::postgres::PgRow) -> Result<UserRecord, sqlx::Error> {
    Ok(UserRecord {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        is_admin: row.try_get("is_admin")?,
        disabled: row.try_get("disabled")?,
        enable_remote_access: row.try_get("enable_remote_access")?,
        allow_media_playback: row.try_get("allow_media_playback")?,
        enable_content_downloading: row.try_get("enable_content_downloading")?,
        enable_live_tv_access: row.try_get("enable_live_tv_access")?,
        enable_live_tv_management: row.try_get("enable_live_tv_management")?,
        restrict_libraries: row.try_get("restrict_libraries")?,
        max_parental_rating: row
            .try_get::<Option<i16>, _>("max_parental_rating")?
            .map(i32::from),
        block_unrated_items: row.try_get("block_unrated_items")?,
        allowed_library_ids: row.try_get("allowed_library_ids")?,
        configuration: row
            .try_get::<sqlx::types::Json<crate::user_settings::UserConfiguration>, _>(
                "configuration",
            )?
            .0,
    })
}

pub async fn get_library(pool: &PgPool, id: Uuid) -> Result<Option<LibraryRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, name, collection_type, locations FROM libraries WHERE id=$1 AND enabled=TRUE",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(library_from_row).transpose()
}

pub async fn get_library_including_disabled(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<LibraryRecord>, sqlx::Error> {
    let row = sqlx::query("SELECT id, name, collection_type, locations FROM libraries WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(library_from_row).transpose()
}

pub async fn library_is_enabled(pool: &PgPool, id: Uuid) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT enabled FROM libraries WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_libraries(
    pool: &PgPool,
    user: &UserRecord,
) -> Result<Vec<LibraryRecord>, sqlx::Error> {
    let visible_item = library_media_visibility_predicate(4, 5, 6);
    let query = format!(
        "SELECT l.id, l.name, l.collection_type, l.locations FROM libraries l WHERE l.enabled=TRUE AND ($1 OR NOT $2 OR EXISTS (SELECT 1 FROM user_library_access a WHERE a.user_id=$3 AND a.library_id=l.id)) AND ($1 OR EXISTS (SELECT 1 FROM items i WHERE i.library_id=l.id AND {visible_item})) ORDER BY lower(l.name), l.id"
    );
    let rows = sqlx::query(&query)
        .bind(user.is_admin)
        .bind(user.restrict_libraries)
        .bind(user.id)
        .bind(user.max_parental_rating.map(|v| v as i16))
        .bind(user.block_unrated_items.clone())
        .bind(user.enable_live_tv_access)
        .fetch_all(pool)
        .await?;
    rows.iter().map(library_from_row).collect()
}

fn library_from_row(row: &sqlx::postgres::PgRow) -> Result<LibraryRecord, sqlx::Error> {
    let locations: Value = row.try_get("locations")?;
    let locations = locations
        .as_array()
        .ok_or_else(|| sqlx::Error::Protocol("library locations is not an array".to_owned()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(PathBuf::from)
                .ok_or_else(|| sqlx::Error::Protocol("library location is not a string".to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LibraryRecord {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        collection_type: row.try_get("collection_type")?,
        locations,
    })
}

pub async fn get_item(pool: &PgPool, id: Uuid) -> Result<Option<ItemRecord>, sqlx::Error> {
    let rating = policy_rating_sql("i");
    let row = sqlx::query(&format!("SELECT i.id, i.library_id, i.parent_id, i.name, i.sort_name, i.item_type, i.path, i.container, i.size_bytes, i.runtime_ticks, i.date_added, i.date_modified, {rating} AS rating, i.overview, i.metadata_json FROM items i WHERE i.id=$1"))
        .bind(id).fetch_optional(pool).await?;
    row.as_ref().map(item_from_row).transpose()
}

pub(crate) async fn item_ancestors(
    pool: &PgPool,
    item: &ItemRecord,
) -> Result<Vec<ItemRecord>, sqlx::Error> {
    let rating = policy_rating_sql("i");
    let rows = sqlx::query(&format!(
        "WITH RECURSIVE lineage(id,parent_id,depth,visited) AS (\
         SELECT id,parent_id,0,ARRAY[id] FROM items WHERE id=$1 AND library_id=$2 \
         UNION ALL SELECT parent.id,parent.parent_id,lineage.depth+1,lineage.visited || parent.id \
         FROM items parent JOIN lineage ON parent.id=lineage.parent_id \
         WHERE parent.library_id=$2 AND lineage.depth<256 AND NOT parent.id=ANY(lineage.visited)) \
         SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,i.item_type,i.path,i.container,\
         i.size_bytes,i.runtime_ticks,i.date_added,i.date_modified,{rating} AS rating,i.overview,i.metadata_json \
         FROM lineage JOIN items i ON i.id=lineage.id WHERE lineage.depth>0 ORDER BY lineage.depth"
    ))
    .bind(item.id)
    .bind(item.library_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(item_from_row).collect()
}

#[derive(Clone, Copy)]
pub(crate) enum ThemeMediaSort {
    SortName,
    Name,
    Random,
}

pub(crate) async fn theme_media_items(
    pool: &PgPool,
    user: &UserRecord,
    library_id: Uuid,
    directory: &std::path::Path,
    sort: ThemeMediaSort,
    descending: bool,
) -> Result<Vec<ItemRecord>, sqlx::Error> {
    let Some(directory_text) = directory.to_str() else {
        return Ok(Vec::new());
    };
    let rating = policy_rating_sql("i");
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,i.item_type,i.path,i.container,\
         i.size_bytes,i.runtime_ticks,i.date_added,i.date_modified,{rating} AS rating,i.overview,i.metadata_json \
         FROM items i JOIN libraries l ON l.id=i.library_id \
         WHERE l.enabled=TRUE AND i.path !~ '(^|/)[.]' AND i.library_id="
    ));
    query
        .push_bind(library_id)
        .push(" AND i.item_type IN ('Audio','Movie','Video','Episode','Trailer','MusicVideo') ");
    push_user_visibility_filters(&mut query, user);
    query.push(" AND ((regexp_replace(i.path,'/[^/]*$','')=").push_bind(directory_text.to_owned())
        .push(" AND regexp_replace(i.path,'^.*/','') ~* '^theme[.]') OR (i.item_type='Audio' AND regexp_replace(i.path,'/[^/]*$','') IN (")
        .push_bind(directory.join("theme-music").to_string_lossy().into_owned()).push(",")
        .push_bind(directory.join("soundtracks").to_string_lossy().into_owned()).push(")) OR (i.item_type<>'Audio' AND regexp_replace(i.path,'/[^/]*$','')=")
        .push_bind(directory.join("backdrops").to_string_lossy().into_owned()).push(")) ORDER BY ");
    query.push(match sort {
        ThemeMediaSort::Name => "i.name",
        ThemeMediaSort::SortName => "i.sort_name",
        ThemeMediaSort::Random => "random()",
    });
    query
        .push(if descending { " DESC" } else { " ASC" })
        .push(",i.id LIMIT 257");
    let rows = query.build().fetch_all(pool).await?;
    rows.iter().map(item_from_row).collect()
}

pub async fn item_navigation_links(
    pool: &PgPool,
    user: &UserRecord,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, ItemNavigationLinks>, sqlx::Error> {
    if item_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder = QueryBuilder::<Postgres>::new(
        "WITH targets AS (SELECT id,parent_id FROM items WHERE id=ANY(",
    );
    builder.push_bind(item_ids.to_vec()).push(
        "::uuid[])), candidate_ids AS (SELECT id FROM targets UNION SELECT parent_id FROM targets \
         UNION SELECT parent.parent_id FROM targets JOIN items parent ON parent.id=targets.parent_id), \
         visible_nodes AS (SELECT i.id,i.name,i.item_type,i.parent_id,i.library_id \
         FROM items i JOIN libraries l ON l.id=i.library_id",
    );
    push_item_conditions(&mut builder, user, &ItemQuery::default(), None, true);
    builder.push(
        " AND i.id IN (SELECT id FROM candidate_ids)) \
         SELECT i.id,i.name,i.item_type,series.id AS series_id,series.name AS series_name,\
         CASE WHEN i.item_type='Episode' THEN season.id WHEN i.item_type='Season' THEN i.id END AS season_id,\
         season.name AS season_name,CASE WHEN i.item_type='Audio' THEN album.id WHEN i.item_type='MusicAlbum' THEN i.id END AS album_id,\
         CASE WHEN i.item_type='Audio' THEN album.name END AS album,artist.id AS artist_id,artist.name AS artist \
         FROM visible_nodes i LEFT JOIN visible_nodes season ON season.id=i.parent_id AND season.item_type='Season' \
         LEFT JOIN visible_nodes series ON series.id=CASE WHEN i.item_type='Episode' THEN COALESCE(season.parent_id,i.parent_id) \
         WHEN i.item_type='Season' THEN i.parent_id END AND series.item_type='Series' AND series.library_id=i.library_id \
         LEFT JOIN visible_nodes album ON album.id=CASE WHEN i.item_type='Audio' THEN i.parent_id WHEN i.item_type='MusicAlbum' THEN i.id END \
         AND album.item_type='MusicAlbum' AND album.library_id=i.library_id \
         LEFT JOIN visible_nodes artist ON artist.id=album.parent_id AND artist.item_type='MusicArtist' AND artist.library_id=i.library_id \
         WHERE i.id=ANY(",
    ).push_bind(item_ids.to_vec()).push("::uuid[])");
    let rows = builder.build().fetch_all(pool).await?;
    let mut links = rows
        .into_iter()
        .map(|row| {
            let id: Uuid = row.try_get("id")?;
            let name: String = row.try_get("name")?;
            let item_type: String = row.try_get("item_type")?;
            let season_name: Option<String> = row.try_get("season_name")?;
            let series_id: Option<Uuid> = row.try_get("series_id")?;
            let season_id: Option<Uuid> = row.try_get("season_id")?;
            let item_index = if item_type == "Episode" {
                parse_episode_index(&name)
            } else if item_type == "Season" || item_type == "MusicAlbum" {
                parse_trailing_index(&name)
            } else if item_type == "Audio" {
                parse_leading_index(&name)
            } else {
                None
            };
            let parent_index = if item_type == "Episode" {
                season_name.as_deref().and_then(parse_trailing_index)
            } else {
                None
            };
            Ok((
                id,
                ItemNavigationLinks {
                    series_id,
                    series_name: row.try_get("series_name")?,
                    season_id,
                    season_name,
                    index_number: item_index,
                    parent_index_number: parent_index,
                    album_id: row.try_get("album_id")?,
                    album: row.try_get("album")?,
                    artist_id: row.try_get("artist_id")?,
                    artist: row.try_get("artist")?,
                    music: None,
                },
            ))
        })
        .collect::<Result<HashMap<_, _>, sqlx::Error>>()?;
    music_credits::enrich_navigation(pool, user, item_ids, &mut links).await?;
    Ok(links)
}

fn parse_episode_index(name: &str) -> Option<i32> {
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    for start in 0..bytes.len().saturating_sub(2) {
        if bytes[start] != b'S' {
            continue;
        }
        let mut separator = start + 1;
        while bytes.get(separator).is_some_and(u8::is_ascii_digit) {
            separator += 1;
        }
        if separator == start + 1 || bytes.get(separator) != Some(&b'E') {
            continue;
        }
        let end = separator + 1;
        let mut index_end = end;
        while bytes.get(index_end).is_some_and(u8::is_ascii_digit) {
            index_end += 1;
        }
        if index_end > end {
            return upper[end..index_end].parse().ok();
        }
    }
    None
}

fn parse_leading_index(name: &str) -> Option<i32> {
    let digits = name
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn parse_trailing_index(name: &str) -> Option<i32> {
    let digits = name
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

pub async fn set_item_runtime_ticks(
    pool: &PgPool,
    run_id: Uuid,
    id: Uuid,
    file_size: Option<i64>,
    file_modified: Option<DateTime<Utc>>,
    runtime_ticks: i64,
) -> Result<bool, sqlx::Error> {
    if runtime_ticks < 0 {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    let result = sqlx::query("UPDATE items SET runtime_ticks=$4 WHERE id=$1 AND size_bytes IS NOT DISTINCT FROM $2 AND date_modified IS NOT DISTINCT FROM $3")
        .bind(id).bind(file_size).bind(file_modified).bind(runtime_ticks).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn library_root_identity(
    pool: &PgPool,
    library_id: Uuid,
    root_path: &Path,
) -> Result<Option<(u64, u64)>, sqlx::Error> {
    let root_path = root_path
        .to_str()
        .ok_or_else(|| sqlx::Error::Protocol("library root path is not valid UTF-8".to_owned()))?;
    let hash = path_hash(root_path);
    let row = sqlx::query(
        "SELECT root_path,device_id,inode FROM library_root_identities WHERE library_id=$1 AND root_path_hash=$2",
    )
    .bind(library_id)
    .bind(hash)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_path: String = row.try_get("root_path")?;
    if stored_path != root_path {
        return Err(sqlx::Error::Protocol(
            "library root path hash collision".to_owned(),
        ));
    }
    let device: String = row.try_get("device_id")?;
    let inode: String = row.try_get("inode")?;
    let device = device.parse::<u64>().map_err(|_| {
        sqlx::Error::Protocol("stored library device identity is invalid".to_owned())
    })?;
    let inode = inode.parse::<u64>().map_err(|_| {
        sqlx::Error::Protocol("stored library inode identity is invalid".to_owned())
    })?;
    Ok(Some((device, inode)))
}

pub async fn list_library_root_identities(
    pool: &PgPool,
) -> Result<Vec<LibraryRootIdentity>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT r.library_id,l.name AS library_name,r.root_path,r.device_id,r.inode,r.last_verified_at FROM library_root_identities r JOIN libraries l ON l.id=r.library_id ORDER BY lower(l.name),r.root_path",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(LibraryRootIdentity {
                library_id: row.try_get("library_id")?,
                library_name: row.try_get("library_name")?,
                root_path: row.try_get("root_path")?,
                device_id: row.try_get("device_id")?,
                inode: row.try_get("inode")?,
                last_verified_at: row.try_get("last_verified_at")?,
            })
        })
        .collect()
}

pub async fn rebind_library_root(
    pool: &PgPool,
    run_id: Uuid,
    request: &RootRebindRequest,
) -> Result<RootRebindResult, sqlx::Error> {
    let root_path = request
        .root_path
        .to_str()
        .ok_or_else(|| sqlx::Error::Protocol("library root path is not valid UTF-8".to_owned()))?;
    let root_hash = path_hash(root_path);
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(RootRebindResult::StaleRun);
    }

    let library = sqlx::query(
        "SELECT id,name,collection_type,locations FROM libraries WHERE id=$1 FOR UPDATE",
    )
    .bind(request.library_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(library) = library else {
        tx.rollback().await?;
        return Ok(RootRebindResult::LibraryMissing);
    };
    let library = library_from_row(&library)?;
    if !library
        .locations
        .iter()
        .any(|location| location == &request.root_path)
    {
        tx.rollback().await?;
        return Ok(RootRebindResult::RootNotConfigured);
    }

    let scan_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM library_scan_state WHERE library_id=$1 FOR UPDATE",
    )
    .bind(request.library_id)
    .fetch_optional(&mut *tx)
    .await?;
    if scan_status.as_deref() == Some("running") {
        tx.rollback().await?;
        return Ok(RootRebindResult::ScanRunning);
    }

    let row = sqlx::query(
        "SELECT root_path,device_id,inode FROM library_root_identities WHERE library_id=$1 AND root_path_hash=$2 FOR UPDATE",
    )
    .bind(request.library_id)
    .bind(&root_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(RootRebindResult::IdentityMissing);
    };
    let stored_path: String = row.try_get("root_path")?;
    if stored_path != root_path {
        tx.rollback().await?;
        return Err(sqlx::Error::Protocol(
            "library root path hash collision".to_owned(),
        ));
    }
    let stored_device: String = row.try_get("device_id")?;
    let stored_inode: String = row.try_get("inode")?;
    if stored_device != request.expected_device_id || stored_inode != request.expected_inode {
        tx.rollback().await?;
        return Ok(RootRebindResult::IdentityChanged);
    }

    let result = if stored_device == request.current_device_id
        && stored_inode == request.current_inode
    {
        sqlx::query("UPDATE library_root_identities SET last_verified_at=NOW() WHERE library_id=$1 AND root_path_hash=$2")
            .bind(request.library_id)
            .bind(root_hash)
            .execute(&mut *tx)
            .await?;
        RootRebindResult::AlreadyCurrent
    } else {
        sqlx::query("UPDATE library_root_identities SET device_id=$3,inode=$4,last_verified_at=NOW() WHERE library_id=$1 AND root_path_hash=$2")
            .bind(request.library_id)
            .bind(root_hash)
            .bind(&request.current_device_id)
            .bind(&request.current_inode)
            .execute(&mut *tx)
            .await?;
        RootRebindResult::Rebound
    };
    tx.commit().await?;
    Ok(result)
}

pub(crate) fn item_from_row(row: &sqlx::postgres::PgRow) -> Result<ItemRecord, sqlx::Error> {
    Ok(ItemRecord {
        id: row.try_get("id")?,
        library_id: row.try_get("library_id")?,
        parent_id: row.try_get("parent_id")?,
        name: row.try_get("name")?,
        sort_name: row.try_get("sort_name")?,
        item_type: row.try_get("item_type")?,
        path: PathBuf::from(row.try_get::<String, _>("path")?),
        container: row.try_get("container")?,
        size_bytes: row.try_get("size_bytes")?,
        runtime_ticks: row.try_get("runtime_ticks")?,
        date_added: row.try_get("date_added")?,
        date_modified: row.try_get("date_modified")?,
        rating: row.try_get::<Option<i16>, _>("rating")?.map(i32::from),
        overview: row.try_get("overview")?,
        metadata_json: row.try_get("metadata_json")?,
    })
}

pub async fn item_visible_to_user(
    pool: &PgPool,
    user: &UserRecord,
    item: &ItemRecord,
) -> Result<bool, sqlx::Error> {
    if item.item_type == "LiveTvChannel" {
        let channel_enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM live_tv_channels tv JOIN live_tv_sources src ON src.id=tv.source_id AND src.library_id=tv.library_id WHERE tv.item_id=$1 AND tv.enabled=TRUE AND src.enabled=TRUE)")
            .bind(item.id)
            .fetch_one(pool)
            .await?;
        if !channel_enabled {
            return Ok(false);
        }
    }
    if !user_policy_allows_item(user, item) {
        return Ok(false);
    }
    let tagged: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM music_tag_artists WHERE artist_id=$1)")
            .bind(item.id)
            .fetch_one(pool)
            .await?;
    if tagged {
        let mut builder =
            QueryBuilder::<Postgres>::new("SELECT EXISTS(SELECT 1 FROM items i WHERE i.id=");
        builder.push_bind(item.id);
        music_tag_artists::push_visibility(&mut builder, user);
        builder.push(")");
        if !builder.build_query_scalar::<bool>().fetch_one(pool).await? {
            return Ok(false);
        }
    }
    if !user.is_admin
        && is_folder_item_type(&item.item_type)
        && !tagged
        && !folder_has_visible_descendant(pool, user, item).await?
    {
        return Ok(false);
    }
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM libraries WHERE id=$1")
        .bind(item.library_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or(false);
    Ok(enabled)
}

pub(crate) fn user_policy_allows_item(user: &UserRecord, item: &ItemRecord) -> bool {
    if item.item_type == "File"
        || item
            .path
            .components()
            .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
    {
        return false;
    }
    if user.is_admin {
        return true;
    }
    if !user.enable_live_tv_access
        && (matches!(item.item_type.as_str(), "LiveTvChannel" | "LiveTvProgram")
            || item.metadata_json["LiveTvRecording"] == Value::Bool(true))
    {
        return false;
    }
    if user.restrict_libraries && !user.allowed_library_ids.contains(&item.library_id) {
        return false;
    }
    if user
        .max_parental_rating
        .is_some_and(|maximum| item.rating.is_some_and(|rating| rating > maximum))
    {
        return false;
    }
    let category = if item.metadata_json["LiveTvRecording"] == Value::Bool(true) {
        Some("LiveTvProgram")
    } else {
        item_category(&item.item_type)
    };
    if item.rating.is_none()
        && category.is_some_and(|category| {
            user.block_unrated_items
                .iter()
                .any(|value| value == category)
        })
    {
        return false;
    }
    true
}

async fn folder_has_visible_descendant(
    pool: &PgPool,
    user: &UserRecord,
    folder: &ItemRecord,
) -> Result<bool, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "WITH RECURSIVE descendants(id,item_type,metadata_json,ancestors,item_path,depth) AS (SELECT child.id,child.item_type,child.metadata_json,ARRAY[child.id],child.path,1 FROM items child WHERE child.library_id=",
    );
    builder
        .push_bind(folder.library_id)
        .push(" AND child.parent_id = ")
        .push_bind(folder.id)
        .push(" UNION ALL SELECT child.id,child.item_type,child.metadata_json,descendants.ancestors || child.id,child.path,descendants.depth + 1 FROM items child JOIN descendants ON child.parent_id=descendants.id WHERE child.library_id=")
        .push_bind(folder.library_id)
        .push(" AND descendants.depth < 256 AND NOT child.id = ANY(descendants.ancestors)) SELECT EXISTS (SELECT 1 FROM descendants d WHERE d.item_type NOT IN ")
        .push(FOLDER_ITEM_TYPES_SQL)
        .push(" AND d.item_type <> 'File' AND d.item_path !~ '(^|/)[.]' AND (d.item_type <> 'LiveTvChannel' OR EXISTS (SELECT 1 FROM live_tv_channels tv JOIN live_tv_sources src ON src.id=tv.source_id AND src.library_id=tv.library_id WHERE tv.item_id=d.id AND tv.enabled=TRUE AND src.enabled=TRUE))");
    if !user.enable_live_tv_access {
        builder.push(" AND d.item_type NOT IN ('LiveTvChannel','LiveTvProgram') AND COALESCE(d.metadata_json->>'LiveTvRecording','false') <> 'true'");
    }
    push_rating_visibility_filters(&mut builder, user, "d");
    builder.push(")");
    builder.build_query_scalar().fetch_one(pool).await
}

pub async fn library_visible_to_user(
    pool: &PgPool,
    user: &UserRecord,
    library_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let visible_item = library_media_visibility_predicate(5, 6, 7);
    let query = format!(
        "SELECT EXISTS (SELECT 1 FROM libraries l WHERE l.id=$1 AND l.enabled=TRUE AND ($2 OR NOT $3 OR EXISTS (SELECT 1 FROM user_library_access a WHERE a.user_id=$4 AND a.library_id=l.id)) AND ($2 OR EXISTS (SELECT 1 FROM items i WHERE i.library_id=l.id AND {visible_item})))"
    );
    sqlx::query_scalar(&query)
        .bind(library_id)
        .bind(user.is_admin)
        .bind(user.restrict_libraries)
        .bind(user.id)
        .bind(user.max_parental_rating.map(|value| value as i16))
        .bind(user.block_unrated_items.clone())
        .bind(user.enable_live_tv_access)
        .fetch_one(pool)
        .await
}

fn library_media_visibility_predicate(
    rating_parameter: usize,
    blocked_parameter: usize,
    live_tv_access_parameter: usize,
) -> String {
    let rating = policy_rating_sql("i");
    format!(
        "{VISIBLE_LIBRARY_MEDIA_BASE_SQL} AND {LIVE_TV_CHANNEL_ENABLED_SQL} AND (${live_tv_access_parameter} OR (i.item_type NOT IN ('LiveTvChannel','LiveTvProgram') AND COALESCE(i.metadata_json->>'LiveTvRecording','false') <> 'true')) AND (${rating_parameter}::SMALLINT IS NULL OR {rating} IS NULL OR {rating} <= ${rating_parameter}) AND ({rating} IS NOT NULL OR NOT COALESCE({UNRATED_CATEGORY_SQL} = ANY(${blocked_parameter}), FALSE))"
    )
}

pub(crate) fn policy_rating_sql(alias: &str) -> String {
    format!(
        "COALESCE((SELECT im.policy_rating_value FROM item_metadata im WHERE im.item_id={alias}.id AND im.provider_key='local-nfo' AND im.policy_rating_scale='US-MPAA-v1'), (SELECT r.policy_rating_value FROM live_tv_recordings r WHERE r.item_id={alias}.id AND r.status='completed' AND r.policy_rating_scale='US-PARENTAL-v1' ORDER BY r.finished_at DESC,r.id DESC LIMIT 1))"
    )
}

pub async fn active_auth_identity(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<(Uuid, UserRecord)>, sqlx::Error> {
    let row = sqlx::query("SELECT t.id AS token_id, u.id, u.username, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id=u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.token_hash=$1 AND t.revoked_at IS NULL AND t.expires_at > NOW() AND u.disabled=FALSE")
        .bind(token_hash).fetch_optional(pool).await?;
    row.map(|r| Ok((r.try_get("token_id")?, user_from_row(&r)?)))
        .transpose()
}

pub async fn active_media_access_identity(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<(Uuid, UserRecord)>, sqlx::Error> {
    let row = sqlx::query("SELECT t.id AS token_id, u.id, u.username, u.is_admin, u.disabled, u.enable_remote_access, u.allow_media_playback, u.enable_content_downloading, u.enable_live_tv_access, u.enable_live_tv_management, u.restrict_libraries, u.configuration,u.max_parental_rating, u.block_unrated_items, COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id=u.id ORDER BY a.library_id), ARRAY[]::uuid[]) AS allowed_library_ids FROM media_access_tokens m JOIN auth_tokens t ON t.id=m.parent_token_id JOIN users u ON u.id=t.user_id WHERE m.token_hash=$1 AND m.expires_at > NOW() AND t.revoked_at IS NULL AND t.expires_at > NOW() AND u.disabled=FALSE")
        .bind(token_hash).fetch_optional(pool).await?;
    row.map(|r| Ok((r.try_get("token_id")?, user_from_row(&r)?)))
        .transpose()
}

pub async fn create_media_access_token(
    pool: &PgPool,
    run_id: Uuid,
    parent_token_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    let parent_active: Option<Uuid> = sqlx::query_scalar("SELECT t.id FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.revoked_at IS NULL AND t.expires_at>NOW() AND u.disabled=FALSE FOR UPDATE OF t")
        .bind(parent_token_id)
        .fetch_optional(&mut *tx)
        .await?;
    if parent_active.is_none() {
        return Ok(None);
    }
    sqlx::query("DELETE FROM media_access_tokens WHERE parent_token_id=$1 AND expires_at<=NOW()")
        .bind(parent_token_id)
        .execute(&mut *tx)
        .await?;
    let expires_at: Option<DateTime<Utc>> = sqlx::query_scalar("INSERT INTO media_access_tokens(token_hash,parent_token_id,expires_at) SELECT $2,t.id,LEAST($3,t.expires_at) FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.revoked_at IS NULL AND t.expires_at>NOW() AND u.disabled=FALSE RETURNING expires_at")
        .bind(parent_token_id)
        .bind(token_hash)
        .bind(expires_at)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(expires_at) = expires_at else {
        return Ok(None);
    };
    // Keep a small number of simultaneously usable tabs while bounding rows
    // attached to a single long-lived browser session.
    sqlx::query("DELETE FROM media_access_tokens WHERE parent_token_id=$1 AND token_hash NOT IN (SELECT token_hash FROM media_access_tokens WHERE parent_token_id=$1 ORDER BY created_at DESC,token_hash DESC LIMIT 8)")
        .bind(parent_token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(expires_at))
}

pub async fn create_auth_token(
    pool: &PgPool,
    run_id: Uuid,
    input: NewAuthToken,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("INSERT INTO auth_tokens(id,user_id,token_hash,expires_at,client,device_name,device_id) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(input.token_id).bind(input.user_id).bind(input.token_hash).bind(input.expires_at).bind(input.client).bind(input.device_name).bind(input.device_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn touch_auth_token(
    pool: &PgPool,
    run_id: Uuid,
    token_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("UPDATE auth_tokens SET last_seen_at=NOW() WHERE id=$1 AND last_seen_at < NOW() - INTERVAL '5 minutes'")
        .bind(token_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn revoke_auth_token(
    pool: &PgPool,
    run_id: Uuid,
    token_hash: &str,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("UPDATE auth_tokens SET revoked_at=NOW() WHERE token_hash=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(token_hash).bind(user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn revoke_user_tokens(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("UPDATE auth_tokens SET revoked_at=NOW() WHERE user_id=$1 AND revoked_at IS NULL")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_auth_sessions(
    pool: &PgPool,
    user_id: Uuid,
    include_all: bool,
) -> Result<Vec<AuthSessionRecord>, sqlx::Error> {
    let rows = sqlx::query("SELECT t.id,t.user_id,u.username,t.client,t.device_name,t.device_id,t.created_at,t.last_seen_at,t.capabilities FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.revoked_at IS NULL AND t.expires_at>NOW() AND ($1 OR t.user_id=$2) ORDER BY t.last_seen_at DESC LIMIT 1000")
        .bind(include_all).bind(user_id).fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            Ok(AuthSessionRecord {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                username: row.try_get("username")?,
                client: row.try_get("client")?,
                device_name: row.try_get("device_name")?,
                device_id: row.try_get("device_id")?,
                created_at: row.try_get("created_at")?,
                last_seen_at: row.try_get("last_seen_at")?,
                capabilities: row.try_get("capabilities")?,
            })
        })
        .collect()
}

/// Resolve the device behind a full or scoped media credential after media
/// authentication. Revocation and expiry are checked again at this lookup.
pub async fn media_auth_device_id(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT t.device_id FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.user_id=$1 AND t.revoked_at IS NULL AND t.expires_at>NOW() AND u.disabled=FALSE AND t.id IN (SELECT id FROM auth_tokens WHERE token_hash=$2 UNION SELECT parent_token_id FROM media_access_tokens WHERE token_hash=$2 AND expires_at>NOW())")
        .bind(user_id).bind(token_hash).fetch_optional(pool).await
}

pub async fn auth_session_by_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<AuthSessionRecord>, sqlx::Error> {
    let row = sqlx::query("SELECT t.id,t.user_id,u.username,t.client,t.device_name,t.device_id,t.created_at,t.last_seen_at,t.capabilities FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE t.token_hash=$1 AND t.revoked_at IS NULL AND t.expires_at>NOW() AND u.disabled=FALSE")
        .bind(token_hash).fetch_optional(pool).await?;
    row.as_ref()
        .map(|row| {
            Ok(AuthSessionRecord {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                username: row.try_get("username")?,
                client: row.try_get("client")?,
                device_name: row.try_get("device_name")?,
                device_id: row.try_get("device_id")?,
                created_at: row.try_get("created_at")?,
                last_seen_at: row.try_get("last_seen_at")?,
                capabilities: row.try_get("capabilities")?,
            })
        })
        .transpose()
}

pub async fn update_auth_session_capabilities(
    pool: &PgPool,
    run_id: Uuid,
    session_id: Uuid,
    user_id: Uuid,
    is_admin: bool,
    capabilities: &serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    let result = sqlx::query("UPDATE auth_tokens SET capabilities=$1 WHERE id=$2 AND ($3 OR user_id=$4) AND revoked_at IS NULL AND expires_at>NOW() AND EXISTS(SELECT 1 FROM users u WHERE u.id=auth_tokens.user_id AND u.disabled=FALSE)")
        .bind(capabilities)
        .bind(session_id)
        .bind(is_admin)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn start_playback_session(
    pool: &PgPool,
    input: PlaybackStartRequest,
) -> Result<PlaybackStartResult, sqlx::Error> {
    let PlaybackStartRequest {
        id,
        run_id,
        user_id,
        item_id,
        device_id,
        device_name,
        client,
        play_method,
        position_ticks,
    } = input;
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(PlaybackStartResult::StaleRun);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("{user_id}:{device_id}"))
        .execute(&mut *tx)
        .await?;
    let existing = sqlx::query(
        "SELECT user_id,item_id,device_id,instance_run_id,ended_at FROM playback_sessions WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        let same_session = row.try_get::<Uuid, _>("user_id")? == user_id
            && row.try_get::<Option<Uuid>, _>("item_id")? == Some(item_id)
            && row.try_get::<String, _>("device_id")? == device_id
            && row.try_get::<Uuid, _>("instance_run_id")? == run_id
            && row
                .try_get::<Option<DateTime<Utc>>, _>("ended_at")?
                .is_none();
        if same_session {
            sqlx::query("UPDATE playback_sessions SET last_activity_at=NOW() WHERE id=$1 AND ended_at IS NULL")
                .bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Ok(if same_session {
            PlaybackStartResult::AlreadyActive
        } else {
            PlaybackStartResult::Conflict
        });
    }
    sqlx::query("UPDATE playback_sessions SET ended_at=NOW(),last_activity_at=NOW() WHERE user_id=$1 AND device_id=$2 AND ended_at IS NULL")
        .bind(user_id).bind(&device_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO playback_sessions(id,instance_run_id,user_id,item_id,device_id,device_name,client,play_method,position_ticks) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(id).bind(run_id).bind(user_id).bind(item_id).bind(device_id).bind(device_name).bind(client).bind(play_method).bind(position_ticks.unwrap_or(0)).execute(&mut *tx).await?;
    let (_, music) = playback_item_details(&mut tx, item_id).await?;
    if music {
        // Public Jellyfin 12 behavior counts each new music playback at
        // start, independently of progress, EOF and a previous play.
        save_music_playback_user_data(&mut tx, user_id, item_id, true).await?;
    } else if let Some(position) = position_ticks {
        save_playback_user_data(&mut tx, user_id, item_id, None, position).await?;
    }
    tx.commit().await?;
    Ok(PlaybackStartResult::Started)
}

pub async fn active_playback_session(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    device_id: &str,
    id: Option<Uuid>,
    item_id: Option<Uuid>,
) -> Result<Option<PlaybackSessionRecord>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    // A client can report progress while its start is committing.
    // Share the start's device lock so this lookup sees the committed row.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("{user_id}:{device_id}"))
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT id,item_id,position_ticks FROM playback_sessions WHERE instance_run_id=$1 AND user_id=$2 AND device_id=$3 AND ended_at IS NULL AND ($4::UUID IS NULL OR id=$4) AND ($5::UUID IS NULL OR item_id=$5) ORDER BY last_activity_at DESC LIMIT 1")
        .bind(run_id).bind(user_id).bind(device_id).bind(id).bind(item_id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    row.as_ref()
        .map(|row| {
            Ok(PlaybackSessionRecord {
                id: row.try_get("id")?,
                item_id: row.try_get("item_id")?,
                position_ticks: row.try_get("position_ticks")?,
            })
        })
        .transpose()
}

pub async fn wait_for_playback_start(
    pool: &PgPool,
    selector: &PlaybackSessionSelector,
) -> Result<Option<PlaybackSessionRecord>, sqlx::Error> {
    let mut active = active_playback_session(
        pool,
        selector.run_id,
        selector.user_id,
        &selector.device_id,
        selector.id,
        selector.item_id,
    )
    .await?;
    // Desktop and web can report progress before a concurrent start takes
    // the transaction lock, with either an empty or explicit session ID.
    // Give an identified start five short chances to commit. Every lookup
    // keeps the same run, user, device, session and item constraints; never
    // create or revive a row here.
    if active.is_none() && (selector.id.is_some() || selector.item_id.is_some()) {
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            active = active_playback_session(
                pool,
                selector.run_id,
                selector.user_id,
                &selector.device_id,
                selector.id,
                selector.item_id,
            )
            .await?;
            if active.is_some() {
                break;
            }
        }
    }
    Ok(active)
}

pub async fn ended_playback_session(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    device_id: &str,
    id: Option<Uuid>,
    item_id: Option<Uuid>,
) -> Result<Option<PlaybackSessionRecord>, sqlx::Error> {
    if id.is_none() && item_id.is_none() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    let row = sqlx::query("SELECT id,item_id,position_ticks FROM playback_sessions WHERE instance_run_id=$1 AND user_id=$2 AND device_id=$3 AND ended_at IS NOT NULL AND ($4::UUID IS NULL OR id=$4) AND ($5::UUID IS NULL OR item_id=$5) ORDER BY ended_at DESC,id LIMIT 1")
        .bind(run_id).bind(user_id).bind(device_id).bind(id).bind(item_id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    row.as_ref()
        .map(|row| {
            Ok(PlaybackSessionRecord {
                id: row.try_get("id")?,
                item_id: row.try_get("item_id")?,
                position_ticks: row.try_get("position_ticks")?,
            })
        })
        .transpose()
}

pub async fn update_playback_session(
    pool: &PgPool,
    selector: PlaybackSessionSelector,
    position_ticks: Option<i64>,
    play_method: Option<&str>,
) -> Result<Option<PlaybackSessionRecord>, sqlx::Error> {
    let PlaybackSessionSelector {
        run_id,
        user_id,
        device_id,
        id,
        item_id,
    } = selector;
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    let selected = sqlx::query("SELECT id,item_id FROM playback_sessions WHERE instance_run_id=$1 AND user_id=$2 AND device_id=$3 AND ended_at IS NULL AND ($4::UUID IS NULL OR id=$4) AND ($5::UUID IS NULL OR item_id=$5) ORDER BY last_activity_at DESC LIMIT 1 FOR UPDATE")
        .bind(run_id).bind(user_id).bind(device_id).bind(id).bind(item_id).fetch_optional(&mut *tx).await?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected_id: Uuid = selected.try_get("id")?;
    let selected_item: Option<Uuid> = selected.try_get("item_id")?;
    let row = sqlx::query("UPDATE playback_sessions SET position_ticks=COALESCE($2,position_ticks),play_method=COALESCE($3,play_method),last_activity_at=NOW() WHERE id=$1 AND ended_at IS NULL RETURNING id,item_id,position_ticks")
        .bind(selected_id).bind(position_ticks).bind(play_method).fetch_optional(&mut *tx).await?;
    if let (Some(item_id), Some(position)) = (selected_item, position_ticks) {
        let (_, music) = playback_item_details(&mut tx, item_id).await?;
        if music {
            save_music_playback_user_data(&mut tx, user_id, item_id, false).await?;
        } else {
            save_playback_user_data(&mut tx, user_id, item_id, None, position).await?;
        }
    }
    tx.commit().await?;
    row.as_ref()
        .map(|row| {
            Ok(PlaybackSessionRecord {
                id: row.try_get("id")?,
                item_id: row.try_get("item_id")?,
                position_ticks: row.try_get("position_ticks")?,
            })
        })
        .transpose()
}

pub async fn finish_playback_session(
    pool: &PgPool,
    selector: PlaybackSessionSelector,
    position_ticks: Option<i64>,
    played_to_completion: Option<bool>,
    persist_user_data: bool,
) -> Result<Option<PlaybackSessionRecord>, sqlx::Error> {
    let PlaybackSessionSelector {
        run_id,
        user_id,
        device_id,
        id,
        item_id,
    } = selector;
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("{user_id}:{device_id}"))
        .execute(&mut *tx)
        .await?;
    let selected = sqlx::query("SELECT id,item_id,position_ticks FROM playback_sessions WHERE instance_run_id=$1 AND user_id=$2 AND device_id=$3 AND ended_at IS NULL AND ($4::UUID IS NULL OR id=$4) AND ($5::UUID IS NULL OR item_id=$5) ORDER BY last_activity_at DESC LIMIT 1 FOR UPDATE")
        .bind(run_id).bind(user_id).bind(device_id).bind(id).bind(item_id).fetch_optional(&mut *tx).await?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected_id: Uuid = selected.try_get("id")?;
    let selected_item: Option<Uuid> = selected.try_get("item_id")?;
    let previous_position: i64 = selected.try_get("position_ticks")?;
    let final_position = position_ticks.unwrap_or(previous_position);
    let mut completed = false;
    let mut saved_at = None;
    if persist_user_data && let Some(item_id) = selected_item {
        let (runtime_ticks, music) = playback_item_details(&mut tx, item_id).await?;
        completed =
            music || playback_completed(final_position, runtime_ticks, played_to_completion);
        let resume_position = if completed { 0 } else { final_position };
        saved_at = Some(if music {
            save_music_playback_user_data(&mut tx, user_id, item_id, false).await?
        } else {
            save_playback_user_data(
                &mut tx,
                user_id,
                item_id,
                completed.then_some(true),
                resume_position,
            )
            .await?
        });
    }
    let row = sqlx::query("UPDATE playback_sessions SET ended_at=NOW(),position_ticks=$2,last_activity_at=NOW(),stop_reported_at=clock_timestamp(),stop_user_data_updated_at=$3,stop_completed=$4 WHERE id=$1 AND ended_at IS NULL RETURNING id,item_id,position_ticks")
        .bind(selected_id).bind(final_position).bind(saved_at).bind(completed).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    row.as_ref()
        .map(|row| {
            Ok(PlaybackSessionRecord {
                id: row.try_get("id")?,
                item_id: row.try_get("item_id")?,
                position_ticks: row.try_get("position_ticks")?,
            })
        })
        .transpose()
}

pub async fn merge_ended_playback_stop(
    pool: &PgPool,
    selector: PlaybackSessionSelector,
    position_ticks: Option<i64>,
    played_to_completion: Option<bool>,
) -> Result<bool, sqlx::Error> {
    let (Some(id), Some(item_id)) = (selector.id, selector.item_id) else {
        return Ok(false);
    };
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, selector.run_id).await? {
        return Ok(false);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("{}:{}", selector.user_id, selector.device_id))
        .execute(&mut *tx)
        .await?;
    // Only a client-reported stop can be corrected. Run recovery, device
    // takeover and shutdown also end rows, but never set this marker.
    let row = sqlx::query("SELECT position_ticks,stop_completed,stop_user_data_updated_at,started_at FROM playback_sessions WHERE id=$1 AND instance_run_id=$2 AND user_id=$3 AND device_id=$4 AND item_id=$5 AND ended_at IS NOT NULL AND stop_reported_at > clock_timestamp()-INTERVAL '10 seconds' FOR UPDATE")
        .bind(id).bind(selector.run_id).bind(selector.user_id).bind(&selector.device_id).bind(item_id)
        .fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let previous: i64 = row.try_get("position_ticks")?;
    let completed_before: bool = row.try_get("stop_completed")?;
    let saved_at: Option<DateTime<Utc>> = row.try_get("stop_user_data_updated_at")?;
    let started_at: DateTime<Utc> = row.try_get("started_at")?;
    let final_position = position_ticks.unwrap_or(previous).max(previous);
    if completed_before || (final_position == previous && played_to_completion != Some(true)) {
        return Ok(false);
    }
    // Lock the saved state before checking its revision. A later progress
    // report or explicit user-data edit owns that state and must win.
    let current_saved_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT updated_at FROM user_item_data WHERE user_id=$1 AND item_id=$2 FOR UPDATE",
    )
    .bind(selector.user_id)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;
    if saved_at.is_none() || saved_at != current_saved_at {
        return Ok(false);
    }
    let newer: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM playback_sessions WHERE user_id=$1 AND item_id=$2 AND id<>$3 AND started_at >= $4)")
        .bind(selector.user_id).bind(item_id).bind(id).bind(started_at).fetch_one(&mut *tx).await?;
    if newer {
        return Ok(false);
    }
    let runtime: Option<i64> = sqlx::query_scalar("SELECT runtime_ticks FROM items WHERE id=$1")
        .bind(item_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    let completed = playback_completed(final_position, runtime, played_to_completion);
    let saved_at = save_playback_user_data(
        &mut tx,
        selector.user_id,
        item_id,
        completed.then_some(true),
        if completed { 0 } else { final_position },
    )
    .await?;
    sqlx::query("UPDATE playback_sessions SET position_ticks=$2,stop_completed=$3,stop_user_data_updated_at=$4 WHERE id=$1")
        .bind(id).bind(final_position).bind(completed).bind(saved_at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

fn playback_completed(position: i64, runtime: Option<i64>, reported: Option<bool>) -> bool {
    reported.unwrap_or(false)
        || runtime.is_some_and(|duration| {
            duration > 0 && (position as i128) * 100 >= (duration as i128) * 95
        })
}

async fn playback_item_details(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    item_id: Uuid,
) -> Result<(Option<i64>, bool), sqlx::Error> {
    let row = sqlx::query("SELECT i.runtime_ticks,i.item_type='Audio' AND lower(l.collection_type)='music' AS music FROM items i JOIN libraries l ON l.id=i.library_id WHERE i.id=$1")
        .bind(item_id).fetch_optional(&mut **tx).await?;
    row.map(|row| Ok((row.try_get("runtime_ticks")?, row.try_get("music")?)))
        .unwrap_or(Ok((None, false)))
}

async fn save_music_playback_user_data(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    user_id: Uuid,
    item_id: Uuid,
    new_play: bool,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO user_item_data(user_id,item_id,played,playback_position_ticks,play_count,last_played_at) VALUES ($1,$2,TRUE,0,CASE WHEN $3 THEN 1 ELSE 0 END,CASE WHEN $3 THEN NOW() ELSE NULL END) ON CONFLICT (user_id,item_id) DO UPDATE SET played=TRUE,playback_position_ticks=0,play_count=CASE WHEN $3 AND user_item_data.play_count<2147483647 THEN user_item_data.play_count+1 ELSE user_item_data.play_count END,last_played_at=CASE WHEN $3 THEN NOW() ELSE user_item_data.last_played_at END,updated_at=NOW() RETURNING updated_at")
        .bind(user_id).bind(item_id).bind(new_play).fetch_one(&mut **tx).await
}

async fn save_playback_user_data(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    user_id: Uuid,
    item_id: Uuid,
    played: Option<bool>,
    position_ticks: i64,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO user_item_data(user_id,item_id,played,playback_position_ticks,play_count,last_played_at) VALUES ($1,$2,COALESCE($3,FALSE),$4,CASE WHEN $3 THEN 1 ELSE 0 END,CASE WHEN $3 THEN NOW() ELSE NULL END) ON CONFLICT (user_id,item_id) DO UPDATE SET played=COALESCE($3,user_item_data.played),playback_position_ticks=$4,play_count=CASE WHEN $3=TRUE AND user_item_data.played=FALSE AND user_item_data.play_count<2147483647 THEN user_item_data.play_count+1 ELSE user_item_data.play_count END,last_played_at=CASE WHEN $3=TRUE AND user_item_data.played=FALSE THEN NOW() ELSE user_item_data.last_played_at END,updated_at=NOW() RETURNING updated_at")
        .bind(user_id).bind(item_id).bind(played).bind(position_ticks).fetch_one(&mut **tx).await
}

pub async fn end_run_playback_sessions(pool: &PgPool, run_id: Uuid) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(0);
    }
    let result = sqlx::query("UPDATE playback_sessions SET ended_at=NOW(),last_activity_at=NOW() WHERE ended_at IS NULL AND instance_run_id=$1")
        .bind(run_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

pub async fn issue_login_failure(
    pool: &PgPool,
    run_id: Uuid,
    bucket_hash: &str,
    threshold: i32,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    if login_capacity_is_locked(&mut tx).await? {
        tx.commit().await?;
        return Ok(true);
    }

    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT TRUE FROM login_throttles WHERE bucket_hash=$1 FOR UPDATE",
    )
    .bind(bucket_hash)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if !exists {
        // Serialize only creation of new buckets. Existing buckets update under
        // their row lock and do not contend on the global capacity gate.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(LOGIN_THROTTLE_CAPACITY_LOCK)
            .execute(&mut *tx)
            .await?;
        if login_capacity_is_locked(&mut tx).await? {
            tx.commit().await?;
            return Ok(true);
        }
        let appeared = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM login_throttles WHERE bucket_hash=$1 FOR UPDATE",
        )
        .bind(bucket_hash)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !appeared {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM login_throttles WHERE bucket_hash <> $1",
            )
            .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
            .fetch_one(&mut *tx)
            .await?;
            if count >= LOGIN_THROTTLE_DATA_BUCKET_LIMIT {
                mark_login_capacity_locked(&mut tx).await?;
                tx.commit().await?;
                return Ok(true);
            }
        }
    }

    sqlx::query("INSERT INTO login_throttles(bucket_hash, failures, window_started_at) VALUES ($1, 1, NOW()) ON CONFLICT(bucket_hash) DO UPDATE SET failures=CASE WHEN login_throttles.window_started_at < NOW() - INTERVAL '15 minutes' THEN 1 ELSE login_throttles.failures + 1 END, window_started_at=CASE WHEN login_throttles.window_started_at < NOW() - INTERVAL '15 minutes' THEN NOW() ELSE login_throttles.window_started_at END, locked_until=CASE WHEN login_throttles.window_started_at < NOW() - INTERVAL '15 minutes' THEN NULL WHEN login_throttles.failures + 1 >= $2 THEN NOW() + INTERVAL '15 minutes' ELSE login_throttles.locked_until END")
        .bind(bucket_hash).bind(threshold).execute(&mut *tx).await?;
    let locked: bool = sqlx::query_scalar(
        "SELECT COALESCE(locked_until > NOW(),FALSE) FROM login_throttles WHERE bucket_hash=$1",
    )
    .bind(bucket_hash)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(locked)
}

pub async fn login_bucket_locked(pool: &PgPool, bucket_hash: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM login_throttles WHERE bucket_hash=$1 AND COALESCE(locked_until > NOW(),FALSE)) OR EXISTS(SELECT 1 FROM login_throttles WHERE bucket_hash=$2)",
    )
    .bind(bucket_hash)
    .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
    .fetch_one(pool)
    .await
}

pub async fn clear_login_bucket(
    pool: &PgPool,
    run_id: Uuid,
    bucket_hash: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("DELETE FROM login_throttles WHERE bucket_hash=$1")
        .bind(bucket_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn prune_login_throttles(pool: &PgPool, run_id: Uuid) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOGIN_THROTTLE_CAPACITY_LOCK)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("DELETE FROM login_throttles WHERE bucket_hash IN (SELECT bucket_hash FROM login_throttles WHERE bucket_hash <> $1 AND window_started_at < NOW() - INTERVAL '1 day' ORDER BY window_started_at LIMIT 1000)")
        .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
        .execute(&mut *tx).await?;
    refresh_login_capacity_marker(&mut tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

pub async fn reconcile_login_throttle_capacity(
    pool: &PgPool,
    run_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOGIN_THROTTLE_CAPACITY_LOCK)
        .execute(&mut *tx)
        .await?;
    refresh_login_capacity_marker(&mut tx).await?;
    tx.commit().await
}

async fn login_capacity_is_locked(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM login_throttles WHERE bucket_hash=$1)")
        .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
        .fetch_one(&mut **tx)
        .await
}

async fn mark_login_capacity_locked(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO login_throttles(bucket_hash,failures,window_started_at) VALUES ($1,0,NOW()) ON CONFLICT(bucket_hash) DO NOTHING")
        .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn refresh_login_capacity_marker(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM login_throttles WHERE bucket_hash <> $1")
            .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
            .fetch_one(&mut **tx)
            .await?;
    if count >= LOGIN_THROTTLE_DATA_BUCKET_LIMIT {
        mark_login_capacity_locked(tx).await?;
    } else {
        sqlx::query("DELETE FROM login_throttles WHERE bucket_hash=$1")
            .bind(LOGIN_THROTTLE_CAPACITY_BUCKET)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub async fn item_user_data(
    pool: &PgPool,
    user_id: Uuid,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, UserItemData>, sqlx::Error> {
    if item_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query("SELECT item_id, played, play_count, is_favorite, playback_position_ticks, last_played_at, rating FROM user_item_data WHERE user_id=$1 AND item_id=ANY($2)")
        .bind(user_id).bind(item_ids).fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("item_id")?,
                UserItemData {
                    played: row.try_get("played")?,
                    play_count: row.try_get("play_count")?,
                    is_favorite: row.try_get("is_favorite")?,
                    playback_position_ticks: row.try_get("playback_position_ticks")?,
                    last_played_at: row.try_get("last_played_at")?,
                    rating: row.try_get("rating")?,
                },
            ))
        })
        .collect()
}

pub(crate) async fn upsert_user_item_data(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    patch: UserItemDataPatch,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query(concat!(
        "INSERT INTO user_item_data(user_id,item_id,played,is_favorite,playback_position_ticks,play_count,last_played_at,rating) ",
        "VALUES ($1,$2,COALESCE($3,FALSE),COALESCE($4,FALSE),COALESCE($5,0),COALESCE($6,0),$7,$8) ",
        "ON CONFLICT (user_id,item_id) DO UPDATE SET ",
        "played=COALESCE($3,user_item_data.played), ",
        "is_favorite=COALESCE($4,user_item_data.is_favorite), ",
        "playback_position_ticks=COALESCE($5,user_item_data.playback_position_ticks), ",
        "play_count=COALESCE($6,user_item_data.play_count), ",
        "last_played_at=COALESCE($7,user_item_data.last_played_at), ",
        "rating=COALESCE($8,user_item_data.rating), updated_at=NOW()"
    ))
    .bind(user_id)
    .bind(item_id)
    .bind(patch.played)
    .bind(patch.favorite)
    .bind(patch.position_ticks)
    .bind(patch.play_count)
    .bind(patch.last_played_at)
    .bind(patch.rating)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_item_played(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    played: bool,
    date_played: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("INSERT INTO user_item_data(user_id,item_id,played,play_count,playback_position_ticks,last_played_at) VALUES ($1,$2,$3,CASE WHEN $3 THEN 1 ELSE 0 END,0,CASE WHEN $3 THEN COALESCE($4,NOW()) ELSE NULL END) ON CONFLICT (user_id,item_id) DO UPDATE SET played=$3,play_count=CASE WHEN $3 THEN CASE WHEN user_item_data.played OR user_item_data.play_count=2147483647 THEN user_item_data.play_count ELSE user_item_data.play_count+1 END ELSE 0 END,playback_position_ticks=0,last_played_at=CASE WHEN $3 THEN COALESCE($4,NOW()) ELSE NULL END,updated_at=NOW()")
        .bind(user_id).bind(item_id).bind(played).bind(date_played).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_item_favorite(
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    favorite: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    sqlx::query("INSERT INTO user_item_data(user_id,item_id,is_favorite) VALUES ($1,$2,$3) ON CONFLICT (user_id,item_id) DO UPDATE SET is_favorite=$3,updated_at=NOW()")
        .bind(user_id).bind(item_id).bind(favorite).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn item_counts(pool: &PgPool, user: &UserRecord) -> Result<ItemCounts, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new(
        "SELECT COUNT(*) FILTER (WHERE i.item_type='Movie')::BIGINT AS movie_count, COUNT(*) FILTER (WHERE i.item_type='Series')::BIGINT AS series_count, COUNT(*) FILTER (WHERE i.item_type='Episode')::BIGINT AS episode_count, COUNT(*) FILTER (WHERE i.item_type IN ('MusicArtist','Artist'))::BIGINT AS artist_count, COUNT(*) FILTER (WHERE i.item_type IN ('MusicAlbum','Album'))::BIGINT AS album_count, COUNT(*) FILTER (WHERE i.item_type IN ('Audio','Song'))::BIGINT AS song_count, COUNT(*) FILTER (WHERE i.item_type IN ('Book','AudioBook','EBook'))::BIGINT AS book_count FROM items i JOIN libraries l ON l.id=i.library_id WHERE l.enabled=TRUE AND i.item_type <> 'File' AND i.path !~ '(^|/)[.]'",
    );
    push_user_visibility_filters(&mut builder, user);
    builder.build_query_as::<ItemCounts>().fetch_one(pool).await
}

pub async fn browse_items(
    pool: &PgPool,
    user: &UserRecord,
    mut query: ItemQuery,
) -> Result<(Vec<ItemRecord>, Option<i64>), sqlx::Error> {
    query.start_index = query.start_index.max(0);
    query.limit = query.limit.clamp(0, 10_000);
    let parent = item_query_parent(pool, query.parent_id).await?;
    if query.parent_id.is_some() && parent.is_none() {
        return Ok((Vec::new(), Some(0)));
    }
    let total = if query.enable_total_record_count {
        Some(run_item_count(pool, user, &query, parent).await?)
    } else {
        None
    };
    let items = run_item_page(pool, user, &query, parent).await?;
    Ok((items, total))
}

async fn item_query_parent(
    pool: &PgPool,
    parent_id: Option<Uuid>,
) -> Result<Option<(Uuid, Option<Uuid>)>, sqlx::Error> {
    Ok(if let Some(parent_id) = parent_id {
        if let Some(library) = get_library(pool, parent_id).await? {
            Some((library.id, None))
        } else {
            sqlx::query("SELECT library_id FROM items WHERE id=$1")
                .bind(parent_id)
                .fetch_optional(pool)
                .await?
                .map(|row| {
                    row.try_get::<Uuid, _>("library_id")
                        .map(|library_id| (library_id, Some(parent_id)))
                })
                .transpose()?
        }
    } else {
        None
    })
}

fn item_cte(parent: Option<(Uuid, Option<Uuid>)>, recursive: bool) -> bool {
    parent.is_some_and(|(_, item_id)| item_id.is_some()) && recursive
}

fn push_item_source(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    query: &ItemQuery,
    parent: Option<(Uuid, Option<Uuid>)>,
) {
    let tree = item_cte(parent, query.recursive);
    let catalog_nodes = !query.artist_ids.is_empty()
        || !query.album_artist_ids.is_empty()
        || !query.contributing_artist_ids.is_empty()
        || !query.exclude_artist_ids.is_empty()
        || query.sort_by.split(',').any(|field| {
            field.trim().eq_ignore_ascii_case("ParentIndexNumber")
                || field.trim().eq_ignore_ascii_case("Album")
                || field.trim().eq_ignore_ascii_case("SeriesSortName")
        });
    push_visible_album_tracks(builder, user);
    if tree {
        builder.push(", tree(id, library_id, path, depth) AS (SELECT i.id, i.library_id, ARRAY[i.id], 0 FROM items i WHERE i.id = ")
            .push_bind(parent.and_then(|p| p.1).expect("CTE parent"))
            .push(" UNION ALL SELECT child.id, child.library_id, tree.path || child.id, tree.depth + 1 FROM items child JOIN tree ON child.parent_id = tree.id WHERE child.library_id=tree.library_id AND NOT child.id = ANY(tree.path)) ");
    }
    if catalog_nodes {
        builder.push(", visible_catalog_nodes AS (SELECT i.id,i.name,i.sort_name,i.library_id,i.parent_id,i.item_type,i.size_bytes,i.date_modified,i.path_hash FROM items i JOIN libraries l ON l.id=i.library_id");
        push_item_conditions(builder, user, &ItemQuery::default(), None, true);
        builder
            .push(" AND i.item_type IN ('Audio','MusicArtist','MusicAlbum','Season','Series')) ");
        if !query.artist_ids.is_empty()
            || !query.album_artist_ids.is_empty()
            || !query.contributing_artist_ids.is_empty()
            || !query.exclude_artist_ids.is_empty()
        {
            builder.push(music_credits::CREDIT_CTES);
        }
    }
}

pub(crate) fn push_visible_album_tracks(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
) {
    // Inline this relation so correlated album/library predicates can use the
    // existing parent index rather than materialize the whole audio catalog.
    // Unused CTEs are pruned for requests that do not need album names.
    builder.push("WITH RECURSIVE visible_album_tracks AS NOT MATERIALIZED (SELECT i.id,i.parent_id,i.library_id,i.size_bytes,i.date_modified,i.path_hash FROM items i JOIN libraries l ON l.id=i.library_id");
    push_item_conditions(builder, user, &ItemQuery::default(), None, true);
    builder.push(" AND i.item_type='Audio') ");
}

fn push_item_conditions(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    query: &ItemQuery,
    parent: Option<(Uuid, Option<Uuid>)>,
    recursive: bool,
) {
    builder
        .push(" WHERE l.enabled=TRUE AND i.item_type <> 'File' AND i.path !~ '(^|/)[.]' AND ")
        .push(LIVE_TV_CHANNEL_ENABLED_SQL)
        .push(" ");
    push_user_visibility_filters(builder, user);
    if !query.item_ids.is_empty() {
        builder
            .push(" AND i.id = ANY(")
            .push_bind(query.item_ids.clone())
            .push(") ");
    }
    if !query.exclude_item_ids.is_empty() {
        builder
            .push(" AND i.id <> ALL(")
            .push_bind(query.exclude_item_ids.clone())
            .push(") ");
    }
    for (ids, source, contributing, exclude) in [
        (&query.artist_ids, "music_credits", false, false),
        (&query.album_artist_ids, "music_album_roles", false, false),
        (&query.contributing_artist_ids, "music_credits", true, false),
        (&query.exclude_artist_ids, "music_credits", false, true),
    ] {
        if !ids.is_empty() {
            builder.push(if exclude {
                " AND NOT EXISTS ("
            } else {
                " AND EXISTS ("
            });
            builder.push(format!("SELECT 1 FROM {source} credit WHERE credit.item_id=i.id AND credit.artist_id=ANY("))
                .push_bind(ids.clone()).push("::uuid[])");
            if contributing {
                builder.push(" AND credit.contributing=TRUE");
            }
            builder.push(") ");
        }
    }
    if let Some((library_id, parent_item_id)) = parent {
        if let Some(parent_id) = parent_item_id {
            if recursive {
                builder.push(" AND EXISTS (SELECT 1 FROM tree t WHERE t.id=i.id AND t.depth > 0)");
            } else {
                builder.push(" AND i.parent_id = ").push_bind(parent_id);
            }
        } else if query.recursive {
            builder.push(" AND i.library_id = ").push_bind(library_id);
        } else {
            builder
                .push(" AND i.library_id = ")
                .push_bind(library_id)
                .push(" AND i.parent_id IS NULL ");
        }
    }
    if let Some(search) = query
        .search_term
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let title = crate::metadata::catalog_sql::title("candidate.item_id");
        let album_title = crate::metadata::catalog_sql::catalog_title("i.id");
        builder
            .push(" AND ((i.item_type<>'MusicAlbum' AND i.search_document @@ plainto_tsquery('simple', ")
            .push_bind(search.to_owned())
            .push(")) OR (i.item_type='Audio' AND i.id IN (SELECT candidate.item_id FROM item_metadata candidate WHERE candidate.search_document @@ plainto_tsquery('simple', ")
            .push_bind(search.to_owned())
            .push(format!(") AND candidate.title={title})) OR (i.item_type='MusicAlbum' AND to_tsvector('simple',COALESCE({album_title},i.name)) @@ plainto_tsquery('simple', "))
            .push_bind(search.to_owned())
            .push("))) ");
    }
    if let Some(exact_name) = query.exact_name.as_deref() {
        let title = crate::metadata::catalog_sql::catalog_title("i.id");
        builder
            .push(format!(" AND lower(COALESCE({title},i.name))=lower("))
            .push_bind(exact_name.to_owned())
            .push(") ");
    }
    if !query.include_item_types.is_empty() {
        builder
            .push(" AND i.item_type = ANY(")
            .push_bind(query.include_item_types.clone())
            .push(") ");
    }
    if !query.media_types.is_empty() {
        let selected = query
            .media_types
            .iter()
            .flat_map(|kind| kind.item_types().iter().copied())
            .collect::<Vec<_>>();
        builder
            .push(" AND (i.item_type = ANY(")
            .push_bind(selected)
            .push(") ");
        if query
            .media_types
            .contains(&crate::library::MediaType::Unknown)
        {
            let known = crate::library::MediaType::KNOWN
                .into_iter()
                .flat_map(|kind| kind.item_types().iter().copied())
                .collect::<Vec<_>>();
            builder
                .push(" OR i.item_type <> ALL(")
                .push_bind(known)
                .push(") ");
        }
        builder.push(") ");
    }
    if let Some(folder) = query.is_folder {
        builder
            .push(if folder {
                " AND i.item_type IN "
            } else {
                " AND i.item_type NOT IN "
            })
            .push(FOLDER_ITEM_TYPES_SQL);
    }
    if let Some(played) = query.is_played {
        builder.push(if played {
            " AND EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id="
        } else {
            " AND NOT EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id="
        });
        builder
            .push_bind(user.id)
            .push(" AND ud.item_id=i.id AND ud.played=TRUE) ");
    }
    if query.is_resumable {
        builder
            .push(" AND EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id=")
            .push_bind(user.id)
            .push(" AND ud.item_id=i.id AND ud.playback_position_ticks>0 AND ud.played=FALSE) ");
    }
    if query.is_favorite {
        builder
            .push(" AND EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id=")
            .push_bind(user.id)
            .push(" AND ud.item_id=i.id AND ud.is_favorite=TRUE) ");
    }
    if let Some(liked) = query.is_liked {
        builder.push(if liked {
            " AND EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id="
        } else {
            " AND NOT EXISTS (SELECT 1 FROM user_item_data ud WHERE ud.user_id="
        });
        builder
            .push_bind(user.id)
            .push(" AND ud.item_id=i.id AND ud.rating>=6.5) ");
    }
    catalog_filters::push_selections(builder, &query.facets);
}

pub(crate) fn push_user_visibility_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
) {
    music_tag_artists::push_visibility(builder, user);
    if user.is_admin {
        return;
    }
    if user.restrict_libraries {
        builder
            .push(" AND i.library_id = ANY(")
            .push_bind(user.allowed_library_ids.clone())
            .push(") ");
    }
    if !user.enable_live_tv_access {
        builder.push(" AND i.item_type NOT IN ('LiveTvChannel','LiveTvProgram') AND COALESCE(i.metadata_json->>'LiveTvRecording','false') <> 'true' ");
    }
    push_rating_visibility_filters(builder, user, "i");
    builder
        .push(" AND (i.item_type NOT IN ")
        .push(FOLDER_ITEM_TYPES_SQL)
        .push(" OR EXISTS(SELECT 1 FROM music_tag_artists tag WHERE tag.artist_id=i.id) OR EXISTS (WITH RECURSIVE descendants(id,item_type,metadata_json,ancestors,item_path,depth) AS (SELECT child.id,child.item_type,child.metadata_json,ARRAY[child.id],child.path,1 FROM items child WHERE child.library_id=i.library_id AND child.parent_id=i.id UNION ALL SELECT child.id,child.item_type,child.metadata_json,descendants.ancestors || child.id,child.path,descendants.depth + 1 FROM items child JOIN descendants ON child.parent_id=descendants.id WHERE child.library_id=i.library_id AND descendants.depth < 256 AND NOT child.id = ANY(descendants.ancestors)) SELECT 1 FROM descendants d WHERE d.item_type NOT IN ")
        .push(FOLDER_ITEM_TYPES_SQL)
        .push(" AND d.item_type <> 'File' AND d.item_path !~ '(^|/)[.]' AND (d.item_type <> 'LiveTvChannel' OR EXISTS (SELECT 1 FROM live_tv_channels tv JOIN live_tv_sources src ON src.id=tv.source_id AND src.library_id=tv.library_id WHERE tv.item_id=d.id AND tv.enabled=TRUE AND src.enabled=TRUE))");
    if !user.enable_live_tv_access {
        builder.push(" AND d.item_type NOT IN ('LiveTvChannel','LiveTvProgram') AND COALESCE(d.metadata_json->>'LiveTvRecording','false') <> 'true'");
    }
    push_rating_visibility_filters(builder, user, "d");
    builder.push(")) ");
}

fn push_rating_visibility_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    alias: &str,
) {
    let rating = policy_rating_sql(alias);
    if let Some(maximum) = user.max_parental_rating {
        builder
            .push(format!(" AND ({rating} IS NULL OR {rating} <= "))
            .push_bind(maximum as i16)
            .push(") ");
    }
    if !user.block_unrated_items.is_empty() {
        let unrated_category_sql = UNRATED_CATEGORY_SQL.replace("i.", &format!("{alias}."));
        builder
            .push(format!(" AND ({rating} IS NOT NULL OR NOT COALESCE("))
            .push(unrated_category_sql)
            .push(" = ANY(")
            .push_bind(user.block_unrated_items.clone())
            .push("), FALSE)) ");
    }
}

async fn run_item_count(
    pool: &PgPool,
    user: &UserRecord,
    query: &ItemQuery,
    parent: Option<(Uuid, Option<Uuid>)>,
) -> Result<i64, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new("");
    push_item_source(&mut builder, user, query, parent);
    builder.push("SELECT COUNT(*)::BIGINT FROM items i JOIN libraries l ON l.id=i.library_id");
    push_item_conditions(&mut builder, user, query, parent, query.recursive);
    builder.build_query_scalar().fetch_one(pool).await
}

async fn run_item_page(
    pool: &PgPool,
    user: &UserRecord,
    query: &ItemQuery,
    parent: Option<(Uuid, Option<Uuid>)>,
) -> Result<Vec<ItemRecord>, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new("");
    push_item_source(&mut builder, user, query, parent);
    let rating = policy_rating_sql("i");
    builder.push(format!("SELECT i.id, i.library_id, i.parent_id, i.name, i.sort_name, i.item_type, i.path, i.container, i.size_bytes, i.runtime_ticks, i.date_added, i.date_modified, {rating} AS rating, i.overview, i.metadata_json FROM items i JOIN libraries l ON l.id=i.library_id"));
    let sort_by_last_played = query.sort_by.split(',').any(|field| {
        field.trim().eq_ignore_ascii_case("LastPlayedDate")
            || field.trim().eq_ignore_ascii_case("DatePlayed")
    });
    if query.is_resumable {
        builder
            .push(" JOIN user_item_data resume_data ON resume_data.user_id=")
            .push_bind(user.id)
            .push(" AND resume_data.item_id=i.id ");
    } else if sort_by_last_played {
        builder
            .push(" LEFT JOIN user_item_data sort_data ON sort_data.user_id=")
            .push_bind(user.id)
            .push(" AND sort_data.item_id=i.id ");
    }
    push_item_conditions(&mut builder, user, query, parent, query.recursive);
    let directions = query.sort_order.split(',').collect::<Vec<_>>();
    let direction = |index: usize| {
        let value = directions.get(index).unwrap_or(&directions[0]).trim();
        if value.eq_ignore_ascii_case("descending") || value.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            "ASC"
        }
    };
    builder.push(" ORDER BY ");
    if query.preserve_item_order && !query.item_ids.is_empty() {
        builder
            .push("array_position(")
            .push_bind(query.item_ids.clone())
            .push(", i.id) ")
            .push(direction(0));
    } else {
        for (index, field) in query.sort_by.split(',').enumerate() {
            if index > 0 {
                builder.push(", ");
            }
            // Only internal expressions enter SQL; request text is never interpolated.
            let field = field.trim().to_ascii_lowercase();
            let column = match field.as_str() {
                "isfolder" => format!("(i.item_type IN {FOLDER_ITEM_TYPES_SQL})"),
                "datecreated" | "dateadded" => "i.date_added".to_owned(),
                "datemodified" => "i.date_modified".to_owned(),
                "dateplayed" if query.is_resumable => "resume_data.last_played_at".to_owned(),
                "dateplayed" => "sort_data.last_played_at".to_owned(),
                "lastplayeddate" if query.is_resumable => {
                    "COALESCE(resume_data.last_played_at,resume_data.updated_at)".to_owned()
                }
                "lastplayeddate" => {
                    "COALESCE(sort_data.last_played_at,sort_data.updated_at)".to_owned()
                }
                "premieredate" => crate::metadata::catalog_sql::premiere_date("i.id"),
                "productionyear" => crate::metadata::catalog_sql::year("i.id"),
                "album" => {
                    let embedded = crate::metadata::catalog_sql::album("i.id");
                    let parent_title = crate::metadata::catalog_sql::catalog_title("album.id");
                    format!(
                        "CASE WHEN i.item_type='Audio' THEN COALESCE({embedded}, \
                    (SELECT COALESCE({parent_title},album.name) FROM visible_catalog_nodes album WHERE album.id=i.parent_id \
                    AND album.library_id=i.library_id AND album.item_type='MusicAlbum')) END"
                    )
                }
                "indexnumber" => crate::metadata::catalog_sql::index_number("i", false),
                "parentindexnumber" => crate::metadata::catalog_sql::index_number("i", true),
                "seriessortname" => {
                    "(SELECT series.sort_name FROM visible_catalog_nodes series \
                     LEFT JOIN visible_catalog_nodes season ON season.id=i.parent_id \
                     AND season.library_id=i.library_id AND season.item_type='Season' \
                     WHERE series.id=CASE WHEN i.item_type='Episode' THEN COALESCE(season.parent_id,i.parent_id) \
                     WHEN i.item_type='Season' THEN i.parent_id END \
                     AND series.library_id=i.library_id AND series.item_type='Series') COLLATE \"C\""
                        .to_owned()
                }
                "name" => {
                    let title = crate::metadata::catalog_sql::catalog_title("i.id");
                    format!(
                        "CASE WHEN i.item_type IN ('Audio','MusicAlbum') THEN lower(COALESCE({title},i.name)) ELSE i.name END"
                    )
                }
                "sortname" => {
                    let music = crate::metadata::catalog_sql::music_sort_name("i");
                    let title = crate::metadata::catalog_sql::catalog_title("i.id");
                    format!(
                        "(CASE WHEN i.item_type='Audio' THEN {music} WHEN i.item_type='MusicAlbum' THEN COALESCE(lower({title}),i.sort_name) ELSE i.sort_name END) COLLATE \"C\""
                    )
                }
                _ => "i.sort_name".to_owned(),
            };
            let order = direction(index);
            let null_order = if matches!(field.as_str(), "indexnumber" | "parentindexnumber")
                && order == "ASC"
            {
                " NULLS FIRST"
            } else {
                " NULLS LAST"
            };
            builder.push(column).push(" ").push(order).push(null_order);
        }
        let needs_name_tie = query.sort_by.split(',').any(|field| {
            field.trim().eq_ignore_ascii_case("IndexNumber")
                || field.trim().eq_ignore_ascii_case("ParentIndexNumber")
                || field.trim().eq_ignore_ascii_case("Album")
                || field.trim().eq_ignore_ascii_case("SeriesSortName")
        });
        let explicit_sort_name = query
            .sort_by
            .split(',')
            .any(|field| field.trim().eq_ignore_ascii_case("SortName"));
        if needs_name_tie && !explicit_sort_name {
            // Nullable album and numeric fields keep the default name tie-break
            // ascending even when the requested primary direction is descending.
            let music = crate::metadata::catalog_sql::music_sort_name("i");
            let title = crate::metadata::catalog_sql::catalog_title("i.id");
            builder.push(format!(
                ", CASE WHEN i.item_type='Audio' THEN lower({music}) WHEN i.item_type='MusicAlbum' THEN COALESCE(lower({title}),i.sort_name) ELSE i.sort_name END ASC"
            ));
        }
    }
    builder.push(", i.id ASC");
    if !query.unlimited {
        builder.push(" LIMIT ").push_bind(query.limit);
    }
    builder.push(" OFFSET ").push_bind(query.start_index);
    let rows = builder.build().fetch_all(pool).await?;
    rows.iter().map(item_from_row).collect()
}

pub async fn insert_library(
    pool: &PgPool,
    run_id: Uuid,
    id: Uuid,
    name: &str,
    collection_type: &str,
    locations: &[PathBuf],
    enabled: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    let encoded: Vec<String> = locations
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    sqlx::query(
        "INSERT INTO libraries(id,name,collection_type,locations,enabled) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(name.trim())
    .bind(collection_type)
    .bind(Json(encoded))
    .bind(enabled)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn remove_library_by_name(
    pool: &PgPool,
    run_id: Uuid,
    name: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    require_active_run(&mut tx, run_id).await?;
    let result = sqlx::query("DELETE FROM libraries WHERE name=$1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrated_item_categories_match_policy_categories() {
        assert_eq!(item_category("Movie"), Some("Movie"));
        assert_eq!(item_category("Episode"), Some("Series"));
        assert_eq!(item_category("Audio"), Some("Music"));
        assert_eq!(item_category("Book"), Some("Book"));
        assert_eq!(item_category("Folder"), None);
        assert_eq!(item_category("Series"), None);
        assert_eq!(item_category("MusicArtist"), None);
        assert_eq!(item_category("MusicAlbum"), None);
        assert_eq!(item_category("Photo"), Some("Other"));
    }

    #[test]
    fn parses_common_episode_season_and_track_numbers() {
        assert_eq!(parse_episode_index("S01E02 - Pilot.mkv"), Some(2));
        assert_eq!(parse_episode_index("S1E2.mkv"), Some(2));
        assert_eq!(parse_episode_index("show_s12e105.mkv"), Some(105));
        assert_eq!(parse_episode_index("episode without an index.mkv"), None);
        assert_eq!(parse_trailing_index("Season 4"), Some(4));
        assert_eq!(parse_leading_index("03 - Track title.flac"), Some(3));
        assert_eq!(parse_leading_index("Track title.flac"), None);
    }

    #[test]
    fn parental_visibility_allows_unrated_only_when_category_is_not_blocked() {
        let allowed = UserRecord {
            id: Uuid::nil(),
            username: "viewer".into(),
            is_admin: false,
            disabled: false,
            enable_remote_access: true,
            allow_media_playback: true,
            enable_content_downloading: true,
            enable_live_tv_access: false,
            enable_live_tv_management: false,
            restrict_libraries: false,
            max_parental_rating: Some(50),
            block_unrated_items: vec![],
            allowed_library_ids: vec![],
            configuration: Default::default(),
        };
        let mut movie = ItemRecord {
            id: Uuid::nil(),
            library_id: Uuid::nil(),
            parent_id: None,
            name: "untagged".into(),
            sort_name: "untagged".into(),
            item_type: "Movie".into(),
            path: PathBuf::new(),
            container: None,
            size_bytes: None,
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: Value::Null,
        };
        assert!(item_category(&movie.item_type).is_some());
        movie.rating = Some(51);
        assert!(movie.rating.unwrap() > allowed.max_parental_rating.unwrap());
        movie.rating = None;
        assert!(
            movie.rating.is_none()
                || movie
                    .rating
                    .is_some_and(|rating| rating <= allowed.max_parental_rating.unwrap())
        );
        let mut blocked = allowed;
        blocked.block_unrated_items.push("Movie".into());
        assert!(
            blocked
                .block_unrated_items
                .iter()
                .any(|category| category == item_category(&movie.item_type).unwrap())
        );
    }

    #[test]
    fn live_tv_policy_blocks_channel_and_recording_items_for_non_admins() {
        let mut user = UserRecord {
            id: Uuid::nil(),
            username: "viewer".into(),
            is_admin: false,
            disabled: false,
            enable_remote_access: true,
            allow_media_playback: true,
            enable_content_downloading: true,
            enable_live_tv_access: false,
            enable_live_tv_management: false,
            restrict_libraries: false,
            max_parental_rating: None,
            block_unrated_items: Vec::new(),
            allowed_library_ids: Vec::new(),
            configuration: Default::default(),
        };
        let mut item = ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "TV channel".to_owned(),
            sort_name: "tv channel".to_owned(),
            item_type: "LiveTvChannel".to_owned(),
            path: PathBuf::from("puffinbox://livetv/source/channel"),
            container: None,
            size_bytes: None,
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: Value::Null,
        };
        assert!(!user_policy_allows_item(&user, &item));
        user.enable_live_tv_access = true;
        assert!(user_policy_allows_item(&user, &item));
        item.item_type = "Movie".to_owned();
        item.metadata_json = serde_json::json!({"LiveTvRecording": true});
        user.enable_live_tv_access = false;
        assert!(!user_policy_allows_item(&user, &item));
        user.is_admin = true;
        assert!(user_policy_allows_item(&user, &item));
    }
}
