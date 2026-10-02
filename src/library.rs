use std::{
    collections::HashSet,
    ffi::CString,
    fs::OpenOptions,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Component,
    path::{Path, PathBuf},
};

use cap_std::fs::{
    Dir as CapDir, OpenOptions as CapOpenOptions, OpenOptionsExt as CapOpenOptionsExt,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryRecord {
    pub id: Uuid,
    pub name: String,
    pub collection_type: String,
    pub locations: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemRecord {
    pub id: Uuid,
    pub library_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub sort_name: String,
    pub item_type: String,
    pub path: PathBuf,
    pub container: Option<String>,
    pub size_bytes: Option<i64>,
    pub runtime_ticks: Option<i64>,
    pub date_added: DateTime<Utc>,
    pub date_modified: Option<DateTime<Utc>>,
    pub rating: Option<i32>,
    pub overview: Option<String>,
    pub metadata_json: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ItemQuery {
    pub parent_id: Option<Uuid>,
    pub search_term: Option<String>,
    pub exact_name: Option<String>,
    pub include_item_types: Vec<String>,
    pub media_types: Vec<MediaType>,
    pub recursive: bool,
    pub start_index: i64,
    pub limit: i64,
    pub enable_total_record_count: bool,
    pub sort_by: String,
    pub sort_order: String,
    pub is_folder: Option<bool>,
    pub is_played: Option<bool>,
    pub is_favorite: bool,
    pub is_resumable: bool,
    pub facets: ItemFacetFilters,
}

#[derive(Clone, Debug, Default)]
pub struct ItemFacetFilters {
    pub genres: Vec<String>,
    pub genre_ids: Vec<Uuid>,
    pub tags: Vec<String>,
    pub official_ratings: Vec<String>,
    pub years: Vec<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaType {
    Unknown,
    Video,
    Audio,
    Photo,
    Book,
}

impl MediaType {
    pub const KNOWN: [Self; 4] = [Self::Video, Self::Audio, Self::Photo, Self::Book];

    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Photo => "Photo",
            Self::Book => "Book",
        }
    }

    pub fn item_types(self) -> &'static [&'static str] {
        match self {
            Self::Unknown => &[],
            Self::Video => &[
                "Movie",
                "Series",
                "Episode",
                "Video",
                "Trailer",
                "MusicVideo",
            ],
            Self::Audio => &["Audio", "MusicAlbum", "MusicArtist"],
            Self::Photo => &["Photo"],
            Self::Book => &["Book", "AudioBook", "EBook"],
        }
    }

    pub fn for_item_type(item_type: &str) -> Option<Self> {
        Self::KNOWN
            .into_iter()
            .find(|kind| kind.item_types().contains(&item_type))
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ScanStatus {
    pub library_id: Uuid,
    pub status: String,
    pub files_seen: i64,
    pub directories_seen: i64,
    pub items_indexed: i64,
    pub errors: i64,
    pub skipped_entries: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStart {
    Started,
    AlreadyRunning,
    LibraryMissing,
    CapacityReached,
    ShuttingDown,
    StaleRun,
}

pub async fn spawn_scan(
    state: crate::AppState,
    library_id: Uuid,
) -> Result<ScanStart, sqlx::Error> {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Ok(ScanStart::ShuttingDown);
    }
    let permit = match state.scan_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return Ok(ScanStart::CapacityReached),
    };
    let Some(library) = crate::db::get_library(&state.db, library_id).await? else {
        return Ok(ScanStart::LibraryMissing);
    };
    let scan_id = Uuid::new_v4();
    match crate::db::claim_library_scan(&state.db, state.run_id, library_id, scan_id).await? {
        crate::db::LibraryScanClaim::Claimed => {}
        crate::db::LibraryScanClaim::AlreadyRunning => return Ok(ScanStart::AlreadyRunning),
        crate::db::LibraryScanClaim::LibraryMissing => return Ok(ScanStart::LibraryMissing),
        crate::db::LibraryScanClaim::StaleRun => return Ok(ScanStart::StaleRun),
    }
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = scan_library(state, library, scan_id).await {
            error!(library_id = %library_id, error = %error, "library scan failed");
        }
    });
    Ok(ScanStart::Started)
}

pub async fn inspect_library_root_identity(path: PathBuf) -> io::Result<(u64, u64)> {
    tokio::task::spawn_blocking(move || {
        let (_directory, device, inode, _modified) = open_scan_root(&path)?;
        Ok((device, inode))
    })
    .await
    .map_err(|error| io::Error::other(format!("root inspection task failed: {error}")))?
}

fn open_scan_root(path: &Path) -> io::Result<(CapDir, u64, u64, Option<std::time::SystemTime>)> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "library root must be an absolute path",
        ));
    }
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(flags);
    let mut file = options.open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "library root path must be normalized",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "library root contains a NUL byte",
            )
        })?;
        // Each component is opened relative to the previously verified
        // directory descriptor, so a replaced parent symlink cannot redirect
        // the configured root outside its stored location.
        let descriptor = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "configured library root is not a directory",
        ));
    }
    let identity = (metadata.dev(), metadata.ino(), metadata.modified().ok());
    Ok((
        CapDir::from_std_file(file),
        identity.0,
        identity.1,
        identity.2,
    ))
}

async fn verify_root_identity(
    pool: &PgPool,
    run_id: Uuid,
    library_id: Uuid,
    path: &str,
    device: u64,
    inode: u64,
) -> Result<bool, sqlx::Error> {
    let hash = crate::db::path_hash(path);
    let mut tx = pool.begin().await?;
    if !crate::db::active_run_is_current(&mut tx, run_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let mut row = sqlx::query("SELECT root_path,device_id,inode FROM library_root_identities WHERE library_id=$1 AND root_path_hash=$2")
        .bind(library_id).bind(&hash).fetch_optional(&mut *tx).await?;
    if row.is_none() {
        sqlx::query("INSERT INTO library_root_identities(library_id,root_path_hash,root_path,device_id,inode) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (library_id,root_path_hash) DO NOTHING")
            .bind(library_id).bind(&hash).bind(path).bind(device.to_string()).bind(inode.to_string()).execute(&mut *tx).await?;
        row = sqlx::query("SELECT root_path,device_id,inode FROM library_root_identities WHERE library_id=$1 AND root_path_hash=$2")
            .bind(library_id).bind(&hash).fetch_optional(&mut *tx).await?;
    }
    let Some(row) = row else {
        tx.rollback().await?;
        return Err(sqlx::Error::Protocol(
            "could not persist library root identity".to_owned(),
        ));
    };
    let stored_path: String = row.try_get("root_path")?;
    if stored_path != path {
        return Err(sqlx::Error::Protocol(
            "library root path hash collision".to_owned(),
        ));
    }
    let stored_device: String = row.try_get("device_id")?;
    let stored_inode: String = row.try_get("inode")?;
    let matches = stored_device == device.to_string() && stored_inode == inode.to_string();
    if matches {
        sqlx::query("UPDATE library_root_identities SET last_verified_at=NOW() WHERE library_id=$1 AND root_path_hash=$2 AND device_id=$3 AND inode=$4")
            .bind(library_id).bind(hash).bind(device.to_string()).bind(inode.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(matches)
}

async fn scan_library(
    state: crate::AppState,
    library: LibraryRecord,
    scan_id: Uuid,
) -> Result<(), sqlx::Error> {
    let library_id = library.id;
    let mut context = ScanContext {
        pool: state.db.clone(),
        run_id: state.run_id,
        library_id,
        scan_id,
        collection_type: library.collection_type,
        counts: ScanCounters::default(),
        last_error: None,
        shutdown_requested: state.shutdown_requested.clone(),
    };
    let run_result = async {
        let roots = &library.locations;
        for (root_index, root) in roots.iter().enumerate() {
            if state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(sqlx::Error::Protocol(
                    "scan cancelled because the server is shutting down".to_owned(),
                ));
            }
            if roots.iter().enumerate().any(|(other_index, other)| {
                root_index != other_index && (root.starts_with(other) || other.starts_with(root))
            }) {
                context
                    .note_error(root, "Overlapping library roots are not scanned")
                    .await?;
                continue;
            }
            let root_text = path_text(root).ok_or_else(|| {
                sqlx::Error::Protocol("library path is not valid UTF-8".to_owned())
            })?;
            let root_for_open = root.clone();
            let opened = tokio::task::spawn_blocking(move || open_scan_root(&root_for_open))
                .await
                .map_err(|error| {
                    sqlx::Error::Protocol(format!("library root open task failed: {error}"))
                })?;
            let (root_dir, device, inode, modified) = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    context.note_error(root, &error.to_string()).await?;
                    continue;
                }
            };
            if !verify_root_identity(
                &context.pool,
                state.run_id,
                library_id,
                &root_text,
                device,
                inode,
            )
            .await?
            {
                context
                    .note_error(
                        root,
                        "Library root identity changed; stale catalogue rows were preserved",
                    )
                    .await?;
                continue;
            }
            let path = root_text;
            let name = root
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("Media");
            let item_id = deterministic_item_id(library_id, &path);
            context
                .upsert_item(ScanItemInput {
                    id: item_id,
                    parent_id: None,
                    path: root,
                    name,
                    item_type: "Folder",
                    container: None,
                    size_bytes: None,
                    modified,
                    is_directory: true,
                })
                .await?;
            context.counts.directories_seen += 1;
            context
                .visit_root(
                    root_dir,
                    root.clone(),
                    item_id,
                    state.shutdown_requested.clone(),
                )
                .await?;
        }
        // Persisted root device/inode identity prevents an offline mount point
        // from masquerading as an intentionally emptied library.
        Ok::<(), sqlx::Error>(())
    }
    .await;

    let finish_result = match run_result {
        Ok(()) => context.finish().await,
        Err(error) => {
            let message = error.to_string();
            let _ = context.mark_failed(&message).await;
            Err(error)
        }
    };
    finish_result?;
    Ok(())
}

#[derive(Default)]
struct ScanCounters {
    files_seen: i64,
    directories_seen: i64,
    items_indexed: i64,
    errors: i64,
    skipped_entries: i64,
}

struct ScanContext {
    pool: PgPool,
    run_id: Uuid,
    library_id: Uuid,
    scan_id: Uuid,
    collection_type: String,
    counts: ScanCounters,
    last_error: Option<String>,
    shutdown_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

enum WalkMessage {
    Item(ScannedItem),
    Error { path: PathBuf, detail: String },
    Skipped,
}

impl ScanContext {
    async fn visit_root(
        &mut self,
        root: CapDir,
        root_path: PathBuf,
        parent_id: Uuid,
        shutdown_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), sqlx::Error> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
        let library_id = self.library_id;
        let collection_type = self.collection_type.clone();
        let worker_shutdown = shutdown_requested.clone();
        let walker = tokio::task::spawn_blocking(move || {
            let context = WalkerContext {
                library_id,
                collection_type: &collection_type,
                sender: &sender,
                shutdown_requested: &worker_shutdown,
            };
            walk_cap_directory(root, root_path, parent_id, 0, &context)
        });
        let mut pending = Vec::with_capacity(128);
        let mut database_error = None;
        while let Some(message) = receiver.recv().await {
            match message {
                WalkMessage::Item(item) => {
                    if item.is_directory {
                        self.counts.directories_seen += 1;
                    } else {
                        self.counts.files_seen += 1;
                    }
                    pending.push(item);
                    if pending.len() >= 128 {
                        if let Err(error) = self.upsert_items(&pending).await {
                            database_error = Some(error);
                            break;
                        }
                        pending.clear();
                    }
                }
                WalkMessage::Error { path, detail } => {
                    if let Err(error) = self.note_error(&path, &detail).await {
                        database_error = Some(error);
                        break;
                    }
                }
                WalkMessage::Skipped => {
                    self.counts.skipped_entries += 1;
                    if self.counts.skipped_entries % 128 == 0
                        && let Err(error) = self.persist_progress().await
                    {
                        database_error = Some(error);
                        break;
                    }
                }
            }
        }
        drop(receiver);
        if database_error.is_none() && shutdown_requested.load(std::sync::atomic::Ordering::Acquire)
        {
            database_error = Some(sqlx::Error::Protocol(
                "scan cancelled because the server is shutting down".to_owned(),
            ));
        }
        if database_error.is_none()
            && !pending.is_empty()
            && let Err(error) = self.upsert_items(&pending).await
        {
            database_error = Some(error);
        }
        let worker_result = walker.await.map_err(|error| {
            sqlx::Error::Protocol(format!("library directory walker failed: {error}"))
        })?;
        if let Some(error) = database_error {
            return Err(error);
        }
        worker_result?;
        Ok(())
    }

    async fn upsert_item(&mut self, input: ScanItemInput<'_>) -> Result<(), sqlx::Error> {
        let item = self.make_item(input)?;
        self.upsert_items(std::slice::from_ref(&item)).await?;
        Ok(())
    }

    fn make_item(&self, input: ScanItemInput<'_>) -> Result<ScannedItem, sqlx::Error> {
        let ScanItemInput {
            id,
            parent_id,
            path,
            name,
            item_type,
            container,
            size_bytes,
            modified,
            is_directory,
        } = input;
        let path = path_text(path).ok_or_else(|| {
            sqlx::Error::Protocol("filesystem path is not valid UTF-8".to_owned())
        })?;
        let path_hash = crate::db::path_hash(&path);
        let sort_name = name.to_lowercase();
        let date_modified = modified.map(chrono::DateTime::<Utc>::from);
        let container = container.or_else(|| {
            Path::new(&path)
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
        });
        Ok(ScannedItem {
            id,
            parent_id,
            name: name.to_owned(),
            sort_name,
            item_type: item_type.to_owned(),
            path,
            path_hash,
            container,
            size_bytes,
            date_modified,
            is_directory,
        })
    }

    async fn lock_current_generation(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
    ) -> Result<(), sqlx::Error> {
        if !crate::db::active_run_is_current(tx, self.run_id).await? {
            return Err(sqlx::Error::Protocol(
                "library scan generation belongs to a stale server run".to_owned(),
            ));
        }
        let current = sqlx::query_scalar::<_, Uuid>(
            "SELECT scan_id FROM library_scan_state WHERE library_id=$1 AND scan_id=$2 AND status='running' FOR UPDATE",
        )
        .bind(self.library_id)
        .bind(self.scan_id)
        .fetch_optional(&mut **tx)
        .await?;
        if current.is_none() {
            return Err(sqlx::Error::Protocol(
                "library scan generation is no longer current".to_owned(),
            ));
        }
        Ok(())
    }

    async fn upsert_items(&mut self, items: &[ScannedItem]) -> Result<(), sqlx::Error> {
        if items.is_empty() {
            return Ok(());
        }
        if self
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(sqlx::Error::Protocol(
                "scan cancelled because the server is shutting down".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        self.lock_current_generation(&mut tx).await?;
        // A recording is not a public catalogue item until the recorder has
        // validated and checksummed it. Its final name is already known, so a
        // scan racing the atomic filesystem link must leave that path alone.
        // Completed recordings are deliberately not excluded: their existing
        // row carries the authoritative parental rating and the upsert below
        // does not overwrite that field.
        let pending_paths = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT RTRIM(root.value, '/') || '/' || recording.relative_path \
             FROM live_tv_recordings recording \
             JOIN libraries library ON library.id=recording.library_id \
             CROSS JOIN LATERAL jsonb_array_elements_text(library.locations) AS root(value) \
             WHERE recording.library_id=$1 AND recording.status IN ('recording','publishing')",
        )
        .bind(self.library_id)
        .fetch_all(&mut *tx)
        .await?;
        let pending_paths = pending_paths.into_iter().collect::<HashSet<_>>();
        let indexable = items
            .iter()
            .filter(|item| !pending_paths.contains(&item.path))
            .collect::<Vec<_>>();
        if indexable.is_empty() {
            tx.commit().await?;
            return Ok(());
        }

        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,container,size_bytes,date_modified,last_seen_scan) ",
        );
        query.push_values(indexable.iter().copied(), |mut row, item| {
            row.push_bind(item.id)
                .push_bind(self.library_id)
                .push_bind(item.parent_id)
                .push_bind(&item.name)
                .push_bind(&item.sort_name)
                .push_bind(&item.item_type)
                .push_bind(&item.path)
                .push_bind(&item.path_hash)
                .push_bind(&item.container)
                .push_bind(item.size_bytes)
                .push_bind(item.date_modified)
                .push_bind(self.scan_id);
        });
        query.push(" ON CONFLICT(library_id,path_hash) DO UPDATE SET parent_id=EXCLUDED.parent_id,name=EXCLUDED.name,sort_name=EXCLUDED.sort_name,item_type=EXCLUDED.item_type,path=EXCLUDED.path,container=EXCLUDED.container,runtime_ticks=CASE WHEN items.size_bytes IS DISTINCT FROM EXCLUDED.size_bytes OR items.date_modified IS DISTINCT FROM EXCLUDED.date_modified THEN NULL ELSE items.runtime_ticks END,size_bytes=EXCLUDED.size_bytes,date_modified=EXCLUDED.date_modified,last_seen_scan=EXCLUDED.last_seen_scan WHERE items.path=EXCLUDED.path RETURNING path");
        let rows = query.build().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let indexed: HashSet<String> = rows
            .iter()
            .map(|row| row.try_get::<String, _>("path"))
            .collect::<Result<_, _>>()?;
        self.counts.items_indexed += indexed.len() as i64;
        for item in items {
            if pending_paths.contains(&item.path) {
                continue;
            }
            if !indexed.contains(&item.path) {
                self.note_error(
                    Path::new(&item.path),
                    "Path hash collision detected; item was not overwritten",
                )
                .await?;
            }
        }
        if self.counts.items_indexed % 128 == 0 {
            self.persist_progress().await?;
        }
        Ok(())
    }

    async fn note_error(&mut self, path: &Path, detail: &str) -> Result<(), sqlx::Error> {
        self.counts.errors += 1;
        let message = format!("{}: {}", path.display(), detail);
        let message: String = message.chars().take(512).collect();
        self.last_error = Some(message.clone());
        warn!(library_id = %self.library_id, error = %message, "library scan encountered an error");
        self.persist_progress().await
    }

    async fn persist_progress(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        self.lock_current_generation(&mut tx).await?;
        sqlx::query("UPDATE library_scan_state SET files_seen=$3,directories_seen=$4,items_indexed=$5,errors=$6,skipped_entries=$7,last_error=$8,updated_at=NOW() WHERE library_id=$1 AND scan_id=$2 AND status='running'")
            .bind(self.library_id).bind(self.scan_id).bind(self.counts.files_seen).bind(self.counts.directories_seen).bind(self.counts.items_indexed).bind(self.counts.errors).bind(self.counts.skipped_entries).bind(&self.last_error).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn mark_failed(&self, message: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        self.lock_current_generation(&mut tx).await?;
        sqlx::query("UPDATE library_scan_state SET status='failed',finished_at=NOW(),last_error=$3,updated_at=NOW() WHERE library_id=$1 AND scan_id=$2 AND status='running'")
            .bind(self.library_id)
            .bind(self.scan_id)
            .bind(message.chars().take(512).collect::<String>())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn finish(&self) -> Result<(), sqlx::Error> {
        if self
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(sqlx::Error::Protocol(
                "scan cancelled because the server is shutting down".to_owned(),
            ));
        }
        if self.counts.errors == 0 {
            // Delete only stale leaves in small autocommitted batches. This
            // avoids materializing IDs or retaining a transaction across a
            // catalogue-sized reconciliation; parent folders become leaves
            // as their stale children are removed.
            loop {
                if self
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(sqlx::Error::Protocol(
                        "scan cancelled because the server is shutting down".to_owned(),
                    ));
                }
                let mut tx = self.pool.begin().await?;
                self.lock_current_generation(&mut tx).await?;
                let deleted = sqlx::query("WITH stale_leaf AS (SELECT i.id FROM items i WHERE i.library_id=$1 AND i.item_type <> 'LiveTvChannel' AND i.last_seen_scan IS DISTINCT FROM $2 AND NOT EXISTS (SELECT 1 FROM items child WHERE child.library_id=i.library_id AND child.parent_id=i.id) ORDER BY i.id LIMIT 1000) DELETE FROM items i USING stale_leaf s WHERE i.id=s.id AND i.library_id=$1 AND i.item_type <> 'LiveTvChannel'")
                    .bind(self.library_id).bind(self.scan_id).execute(&mut *tx).await?.rows_affected();
                tx.commit().await?;
                if deleted == 0 {
                    break;
                }
            }
        }
        let status = if self.counts.errors == 0 {
            "completed"
        } else {
            "completed_with_errors"
        };
        let mut tx = self.pool.begin().await?;
        self.lock_current_generation(&mut tx).await?;
        let updated = sqlx::query("UPDATE library_scan_state SET status=$3,files_seen=$4,directories_seen=$5,items_indexed=$6,errors=$7,skipped_entries=$8,finished_at=NOW(),last_error=$9,updated_at=NOW() WHERE library_id=$1 AND scan_id=$2 AND status='running'")
            .bind(self.library_id).bind(self.scan_id).bind(status).bind(self.counts.files_seen).bind(self.counts.directories_seen).bind(self.counts.items_indexed).bind(self.counts.errors).bind(self.counts.skipped_entries).bind(&self.last_error).execute(&mut *tx).await?.rows_affected();
        if updated != 1 {
            tx.rollback().await?;
            return Err(sqlx::Error::Protocol(
                "library scan generation stopped before completion".to_owned(),
            ));
        }
        tx.commit().await?;
        info!(library_id = %self.library_id, status, files_seen = self.counts.files_seen, directories_seen = self.counts.directories_seen, indexed = self.counts.items_indexed, errors = self.counts.errors, "library scan finished");
        Ok(())
    }
}

struct ScannedItem {
    id: Uuid,
    parent_id: Option<Uuid>,
    name: String,
    sort_name: String,
    item_type: String,
    path: String,
    path_hash: String,
    container: Option<String>,
    size_bytes: Option<i64>,
    date_modified: Option<DateTime<Utc>>,
    is_directory: bool,
}

struct ScanItemInput<'a> {
    id: Uuid,
    parent_id: Option<Uuid>,
    path: &'a Path,
    name: &'a str,
    item_type: &'a str,
    container: Option<String>,
    size_bytes: Option<i64>,
    modified: Option<std::time::SystemTime>,
    is_directory: bool,
}

const MAX_SCAN_DEPTH: usize = 256;

struct WalkerContext<'a> {
    library_id: Uuid,
    collection_type: &'a str,
    sender: &'a tokio::sync::mpsc::Sender<WalkMessage>,
    shutdown_requested: &'a std::sync::atomic::AtomicBool,
}

fn walk_cap_directory(
    directory: CapDir,
    directory_path: PathBuf,
    parent_id: Uuid,
    depth: usize,
    context: &WalkerContext<'_>,
) -> io::Result<()> {
    if context
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Ok(());
    }
    if depth >= MAX_SCAN_DEPTH {
        return emit_walk_message(
            context.sender,
            WalkMessage::Error {
                path: directory_path,
                detail: format!("maximum library directory depth of {MAX_SCAN_DEPTH} reached"),
            },
        );
    }

    let entries = match directory.read_dir(".") {
        Ok(entries) => entries,
        Err(error) => {
            return emit_walk_message(
                context.sender,
                WalkMessage::Error {
                    path: directory_path,
                    detail: error.to_string(),
                },
            );
        }
    };

    for entry in entries {
        if context
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                emit_walk_message(
                    context.sender,
                    WalkMessage::Error {
                        path: directory_path.clone(),
                        detail: error.to_string(),
                    },
                )?;
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            emit_walk_message(
                context.sender,
                WalkMessage::Error {
                    path: directory_path.join(&name),
                    detail: "filesystem entry name is not valid UTF-8".to_owned(),
                },
            )?;
            continue;
        };

        if name_text.starts_with('.') {
            emit_walk_message(context.sender, WalkMessage::Skipped)?;
            continue;
        }

        let entry_path = directory_path.join(name_text);
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                emit_walk_message(
                    context.sender,
                    WalkMessage::Error {
                        path: entry_path,
                        detail: error.to_string(),
                    },
                )?;
                continue;
            }
        };
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            emit_walk_message(context.sender, WalkMessage::Skipped)?;
            continue;
        }

        let mut options = CapOpenOptions::new();
        options.read(true);
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if kind.is_dir() {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        options.custom_flags(flags);
        let opened = match entry.open_with(&options) {
            Ok(opened) => opened,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                match entry.file_type() {
                    Ok(current)
                        if current.is_symlink() || (!current.is_dir() && !current.is_file()) =>
                    {
                        emit_walk_message(context.sender, WalkMessage::Skipped)?;
                        continue;
                    }
                    Ok(_) => {
                        emit_walk_message(
                            context.sender,
                            WalkMessage::Error {
                                path: entry_path,
                                detail: "entry changed while it was being opened".to_owned(),
                            },
                        )?;
                        continue;
                    }
                    Err(check_error) => {
                        emit_walk_message(
                            context.sender,
                            WalkMessage::Error {
                                path: entry_path,
                                detail: check_error.to_string(),
                            },
                        )?;
                        continue;
                    }
                }
            }
            Err(error) => {
                emit_walk_message(
                    context.sender,
                    WalkMessage::Error {
                        path: entry_path,
                        detail: error.to_string(),
                    },
                )?;
                continue;
            }
        };
        let metadata = match opened.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                emit_walk_message(
                    context.sender,
                    WalkMessage::Error {
                        path: entry_path,
                        detail: error.to_string(),
                    },
                )?;
                continue;
            }
        };

        if kind.is_dir() && metadata.is_dir() {
            let item_id = deterministic_item_id(
                context.library_id,
                &path_text(&entry_path).unwrap_or_default(),
            );
            let modified = metadata
                .modified()
                .ok()
                .map(cap_std::time::SystemTime::into_std);
            let item = scanned_item(ScanItemInput {
                id: item_id,
                parent_id: Some(parent_id),
                path: &entry_path,
                name: name_text,
                item_type: directory_item_type(context.collection_type, depth),
                container: None,
                size_bytes: None,
                modified,
                is_directory: true,
            });
            emit_walk_message(context.sender, item)?;
            if context.sender.is_closed() {
                return Ok(());
            }
            let child_directory = CapDir::from_std_file(opened.into_std());
            walk_cap_directory(child_directory, entry_path, item_id, depth + 1, context)?;
        } else if kind.is_file() && metadata.is_file() {
            let Some((item_type, container)) = classify_file(name_text, context.collection_type)
            else {
                emit_walk_message(context.sender, WalkMessage::Skipped)?;
                continue;
            };
            let size = i64::try_from(metadata.len()).ok();
            let modified = metadata
                .modified()
                .ok()
                .map(cap_std::time::SystemTime::into_std);
            let item_id = deterministic_item_id(
                context.library_id,
                &path_text(&entry_path).unwrap_or_default(),
            );
            let item = scanned_item(ScanItemInput {
                id: item_id,
                parent_id: Some(parent_id),
                path: &entry_path,
                name: name_text,
                item_type,
                container,
                size_bytes: size,
                modified,
                is_directory: false,
            });
            emit_walk_message(context.sender, item)?;
        } else {
            // The entry changed type after enumeration. Treat it as a scan
            // error so stale descendants are not reconciled away.
            emit_walk_message(
                context.sender,
                WalkMessage::Error {
                    path: entry_path,
                    detail: "entry changed type while it was being opened".to_owned(),
                },
            )?;
        }
    }
    Ok(())
}

fn scanned_item(input: ScanItemInput<'_>) -> WalkMessage {
    let ScanItemInput {
        id,
        parent_id,
        path,
        name,
        item_type,
        container,
        size_bytes,
        modified,
        is_directory,
    } = input;
    let Some(path) = path_text(path) else {
        return WalkMessage::Error {
            path: path.to_path_buf(),
            detail: "filesystem path is not valid UTF-8".to_owned(),
        };
    };
    let path_hash = crate::db::path_hash(&path);
    let date_modified = modified.map(chrono::DateTime::<Utc>::from);
    let container = container.or_else(|| {
        Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
    });
    WalkMessage::Item(ScannedItem {
        id,
        parent_id,
        name: name.to_owned(),
        sort_name: name.to_lowercase(),
        item_type: item_type.to_owned(),
        path,
        path_hash,
        container,
        size_bytes,
        date_modified,
        is_directory,
    })
}

fn emit_walk_message(
    sender: &tokio::sync::mpsc::Sender<WalkMessage>,
    message: WalkMessage,
) -> io::Result<()> {
    sender
        .blocking_send(message)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "library scan consumer stopped"))
}

fn path_text(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

fn deterministic_item_id(library_id: Uuid, path: &str) -> Uuid {
    // Preserve UUID v5 identity without enabling uuid's BSD-3-Clause sha1_smol dependency.
    let mut hasher = Sha1::new();
    hasher.update(library_id.as_bytes());
    hasher.update(path.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn classify_file(filename: &str, collection_type: &str) -> Option<(&'static str, Option<String>)> {
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let video = [
        "mkv", "mp4", "m4v", "mov", "avi", "webm", "mpeg", "mpg", "ts", "m2ts", "wmv", "flv",
    ];
    let audio = [
        "mp3", "m4a", "aac", "flac", "wav", "ogg", "opus", "wma", "aiff",
    ];
    let image = [
        "jpg", "jpeg", "png", "gif", "webp", "tif", "tiff", "bmp", "heic", "avif",
    ];
    let book = ["epub", "mobi", "azw", "azw3", "pdf", "cbr", "cbz"];
    let item_type = if video.contains(&extension.as_str()) {
        match collection_type.to_ascii_lowercase().as_str() {
            "movies" => "Movie",
            "tvshows" => "Episode",
            _ => "Video",
        }
    } else if audio.contains(&extension.as_str()) {
        "Audio"
    } else if image.contains(&extension.as_str()) {
        "Photo"
    } else if book.contains(&extension.as_str()) {
        "Book"
    } else {
        return None;
    };
    Some((item_type, (!extension.is_empty()).then_some(extension)))
}

fn directory_item_type(collection_type: &str, depth: usize) -> &'static str {
    match (collection_type.to_ascii_lowercase().as_str(), depth) {
        ("tvshows", 0) => "Series",
        ("tvshows", 1) => "Season",
        ("music", 0) => "MusicArtist",
        ("music", 1) => "MusicAlbum",
        _ => "Folder",
    }
}

pub async fn scan_status(pool: &PgPool) -> Result<Vec<ScanStatus>, sqlx::Error> {
    let rows = sqlx::query("SELECT library_id,status,files_seen,directories_seen,items_indexed,errors,skipped_entries,started_at,finished_at,last_error FROM library_scan_state ORDER BY updated_at DESC")
        .fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            Ok(ScanStatus {
                library_id: row.try_get("library_id")?,
                status: row.try_get("status")?,
                files_seen: row.try_get("files_seen")?,
                directories_seen: row.try_get("directories_seen")?,
                items_indexed: row.try_get("items_indexed")?,
                errors: row.try_get("errors")?,
                skipped_entries: row.try_get("skipped_entries")?,
                started_at: row.try_get("started_at")?,
                finished_at: row.try_get("finished_at")?,
                last_error: row.try_get("last_error")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod scanner_tests {
    use super::*;

    #[test]
    fn deterministic_item_id_preserves_uuid_v5_output() {
        let namespace = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        assert_eq!(
            deterministic_item_id(namespace, "www.widgets.com").to_string(),
            "21f7f8de-8051-5b89-8680-0195ef798b6a"
        );
    }

    #[test]
    fn capability_walker_indexes_regular_files_and_skips_symlink_targets() {
        use std::os::unix::fs::symlink;

        let unique = Uuid::new_v4().to_string();
        let base = std::env::temp_dir().join(format!("puffinbox-scan-{unique}"));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(root.join(".private")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("direct.mp3"), b"audio").unwrap();
        std::fs::write(root.join("nested/movie.mkv"), b"video").unwrap();
        std::fs::write(root.join(".env"), b"secret").unwrap();
        std::fs::write(root.join("movie.nfo"), b"sidecar").unwrap();
        std::fs::write(root.join("captions.srt"), b"sidecar").unwrap();
        std::fs::write(root.join("backup.mkv.bak"), b"backup").unwrap();
        std::fs::write(root.join("README"), b"text").unwrap();
        std::fs::write(root.join(".private/secret.mkv"), b"hidden").unwrap();
        std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        symlink(&root, base.join("root-alias")).unwrap();
        let fifo = root.join("pipe");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(open_scan_root(&base.join("root-alias")).is_err());

        let (root_dir, _, _, _) = open_scan_root(&root).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let library_id = Uuid::new_v4();
        let shutdown_requested = std::sync::atomic::AtomicBool::new(false);
        let context = WalkerContext {
            library_id,
            collection_type: "mixed",
            sender: &sender,
            shutdown_requested: &shutdown_requested,
        };
        walk_cap_directory(root_dir, root.clone(), Uuid::new_v4(), 0, &context).unwrap();
        drop(sender);

        let mut paths = Vec::new();
        let mut skipped = 0;
        while let Some(message) = receiver.blocking_recv() {
            match message {
                WalkMessage::Item(item) => paths.push(item.path),
                WalkMessage::Skipped => skipped += 1,
                WalkMessage::Error { path, detail } => {
                    panic!("unexpected scan error at {}: {detail}", path.display())
                }
            }
        }
        paths.sort();
        assert_eq!(
            paths,
            vec![
                root.join("direct.mp3").to_str().unwrap().to_owned(),
                root.join("nested").to_str().unwrap().to_owned(),
                root.join("nested/movie.mkv").to_str().unwrap().to_owned(),
            ]
        );
        assert_eq!(skipped, 8);
        assert!(paths.iter().all(|path| !path.contains("secret.txt")));
        assert!(paths.iter().all(|path| !path.contains(".private")));
        assert_eq!(
            classify_file("recognized.mkv", "movies").unwrap().0,
            "Movie"
        );
        assert!(classify_file("movie.nfo", "movies").is_none());
        assert!(classify_file("captions.srt", "movies").is_none());
        assert!(classify_file("backup.mkv.bak", "movies").is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn maps_tv_and_music_directory_hierarchies_to_browsable_item_types() {
        assert_eq!(directory_item_type("tvshows", 0), "Series");
        assert_eq!(directory_item_type("TVShows", 1), "Season");
        assert_eq!(directory_item_type("tvshows", 2), "Folder");
        assert_eq!(directory_item_type("music", 0), "MusicArtist");
        assert_eq!(directory_item_type("music", 1), "MusicAlbum");
        assert_eq!(directory_item_type("music", 2), "Folder");
        assert_eq!(directory_item_type("movies", 0), "Folder");
    }
}
