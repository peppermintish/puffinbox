//! Capability-based resolution for catalog file paths.

use std::{
    fs::OpenOptions as StdOpenOptions,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt as StdOpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
    time::SystemTime,
};

use cap_std::fs::{Dir, File, OpenOptions, OpenOptionsExt};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{ApiError, library::ItemRecord};

#[derive(Clone)]
pub(super) struct ResolvedMedia {
    pub item: ItemRecord,
    pub root: Arc<Dir>,
    /// The containing directory captured during resolution. Sibling asset
    /// reads use this held descriptor so a path replacement cannot redirect
    /// them to another catalog directory between authorization and open.
    pub parent_directory: Option<Arc<Dir>>,
    pub relative: PathBuf,
    /// Canonical path for external read-only tools such as ffprobe/FFmpeg.
    /// Direct HTTP reads use `root.open(relative)` instead.
    pub absolute_path: PathBuf,
    /// Whether the initially opened descriptor matches the catalog's file
    /// identity. Cached duration metadata is only valid for this identity.
    pub catalog_identity_matches: bool,
}

pub(crate) struct OpenedMedia {
    pub file: std::fs::File,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub modified_utc: Option<DateTime<Utc>>,
    pub catalog_identity_matches: bool,
}

pub(super) struct ResolvedDirectory {
    directory: Arc<Dir>,
}

impl ResolvedDirectory {
    fn open_child_file(&self, name: &std::ffi::OsStr) -> io::Result<File> {
        open_regular_file_no_symlinks(&self.directory, Path::new(name))
    }
}

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)] // Reserved for the approved metadata-sidecar reader integration.
pub(crate) enum AdjacentFileRead {
    Missing,
    Unsafe,
    Content(Vec<u8>),
}

impl ResolvedMedia {
    pub fn open_file(&self) -> io::Result<File> {
        let file = if let (Some(parent), Some(name)) =
            (&self.parent_directory, self.relative.file_name())
        {
            open_regular_file_no_symlinks(parent, Path::new(name))?
        } else {
            open_regular_file_no_symlinks(&self.root, &self.relative)?
        };
        if !metadata_matches_known_catalog_fields(&self.item, &file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "media file no longer matches the catalog",
            ));
        }
        Ok(file)
    }

    pub fn open_relative_file(&self, relative: &Path) -> io::Result<File> {
        open_regular_file_no_symlinks(&self.root, relative)
    }

    pub fn open_relative_dir(&self, relative: &Path) -> io::Result<Dir> {
        open_directory_no_symlinks(&self.root, relative)
    }

    pub fn metadata_matches_catalog(&self, metadata: &cap_std::fs::Metadata) -> bool {
        metadata_matches_catalog(&self.item, metadata)
    }
}

fn open_directory_no_symlinks(root: &Dir, relative: &Path) -> io::Result<Dir> {
    let mut directory = root.try_clone()?;
    for component in relative.components() {
        let name = match component {
            Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "media path contains a non-normal component",
                ));
            }
        };
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        let file = directory.open_with(name, &options)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "media path component is not a directory",
            ));
        }
        directory = Dir::from_std_file(file.into_std());
    }
    Ok(directory)
}

fn open_regular_file_no_symlinks(root: &Dir, relative: &Path) -> io::Result<File> {
    let mut components = relative.components().peekable();
    let mut directory = root.try_clone()?;
    while let Some(component) = components.next() {
        let name = match component {
            Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "media path contains a non-normal component",
                ));
            }
        };
        if components.peek().is_some() {
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(
                libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            );
            let file = directory.open_with(name, &options)?;
            if !file.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "media path component is not a directory",
                ));
            }
            directory = Dir::from_std_file(file.into_std());
            continue;
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let file = directory.open_with(name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media input is not a regular file",
            ));
        }
        return Ok(file);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "media path is empty",
    ))
}

fn metadata_matches_catalog(item: &ItemRecord, metadata: &cap_std::fs::Metadata) -> bool {
    metadata.is_file()
        && item
            .size_bytes
            .is_some_and(|size| i64::try_from(metadata.len()).ok() == Some(size))
        && item.date_modified.as_ref().is_some_and(|expected| {
            modified_utc(metadata)
                .is_some_and(|actual| actual.timestamp_micros() == expected.timestamp_micros())
        })
}

pub(super) fn modified_utc(metadata: &cap_std::fs::Metadata) -> Option<DateTime<Utc>> {
    let modified = metadata.modified().ok()?.into_std();
    let timestamp_micros = DateTime::<Utc>::from(modified).timestamp_micros();
    DateTime::from_timestamp_micros(timestamp_micros)
}

fn metadata_matches_known_catalog_fields(
    item: &ItemRecord,
    metadata: &cap_std::fs::Metadata,
) -> bool {
    metadata.is_file()
        && item
            .size_bytes
            .is_none_or(|size| i64::try_from(metadata.len()).ok() == Some(size))
        && item.date_modified.as_ref().is_none_or(|expected| {
            modified_utc(metadata)
                .is_some_and(|actual| actual.timestamp_micros() == expected.timestamp_micros())
        })
}

fn filesystem_permits() -> &'static Arc<Semaphore> {
    static FILESYSTEM_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    FILESYSTEM_PERMITS.get_or_init(|| Arc::new(Semaphore::new(16)))
}

pub(super) fn filesystem_permit() -> Result<OwnedSemaphorePermit, ApiError> {
    filesystem_permits()
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)
}

pub(super) fn open_in_blocking(media: &ResolvedMedia) -> Result<OpenedMedia, ApiError> {
    let file = media.open_file().map_err(|_| ApiError::NotFound)?;
    let metadata = file.metadata().map_err(|_| ApiError::NotFound)?;
    if !metadata_matches_known_catalog_fields(&media.item, &metadata) {
        return Err(ApiError::NotFound);
    }
    let size = metadata.len();
    let modified = metadata.modified().ok().map(|value| value.into_std());
    let modified_utc = modified.and_then(|value| {
        let timestamp_micros = DateTime::<Utc>::from(value).timestamp_micros();
        DateTime::from_timestamp_micros(timestamp_micros)
    });
    let catalog_identity_matches = media.metadata_matches_catalog(&metadata);
    Ok(OpenedMedia {
        file: file.into_std(),
        size,
        modified,
        modified_utc,
        catalog_identity_matches,
    })
}

pub(super) async fn open_media(media: ResolvedMedia) -> Result<OpenedMedia, ApiError> {
    let permit = filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        open_in_blocking(&media)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

#[allow(dead_code)] // Used by read_adjacent_file, which is wired with metadata routes.
pub(super) async fn read_relative_bounded(
    media: ResolvedMedia,
    relative: PathBuf,
    max_bytes: usize,
) -> Result<AdjacentFileRead, ApiError> {
    let permit = filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Some(name) = relative.file_name().filter(|name| !name.is_empty()) else {
            return Ok(AdjacentFileRead::Unsafe);
        };
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Ok(AdjacentFileRead::Unsafe);
        }
        let parent = relative.parent().unwrap_or_else(|| Path::new("."));
        let directory = match media.open_relative_dir(parent) {
            Ok(directory) => directory,
            Err(error) if is_unsafe_asset_path(&error) => {
                return Ok(AdjacentFileRead::Unsafe);
            }
            // The media file was already resolved under the same root. A
            // disappearing ancestor is a stale/storage condition, not proof
            // that the requested sidecar is absent.
            Err(_) => return Err(ApiError::Unavailable),
        };
        let file = match open_regular_file_no_symlinks(&directory, Path::new(name)) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(AdjacentFileRead::Missing);
            }
            Err(error) if is_unsafe_asset_path(&error) => return Ok(AdjacentFileRead::Unsafe),
            Err(_) => return Err(ApiError::Unavailable),
        };
        read_bounded_open_file(file, max_bytes)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

pub(super) async fn read_directory_child_bounded(
    directory: ResolvedDirectory,
    child_name: PathBuf,
    max_bytes: usize,
) -> Result<AdjacentFileRead, ApiError> {
    let permit = filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Some(name) = child_name.file_name().filter(|name| !name.is_empty()) else {
            return Ok(AdjacentFileRead::Unsafe);
        };
        if child_name.components().count() != 1
            || !matches!(child_name.components().next(), Some(Component::Normal(_)))
        {
            return Ok(AdjacentFileRead::Unsafe);
        }
        let file = match directory.open_child_file(name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(AdjacentFileRead::Missing);
            }
            Err(error) if is_unsafe_asset_path(&error) => return Ok(AdjacentFileRead::Unsafe),
            Err(_) => return Err(ApiError::Unavailable),
        };
        read_bounded_open_file(file, max_bytes)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

pub(super) async fn read_adjacent_bounded(
    media: ResolvedMedia,
    child_name: PathBuf,
    max_bytes: usize,
) -> Result<AdjacentFileRead, ApiError> {
    let permit = filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Some(name) = child_name.file_name().filter(|name| !name.is_empty()) else {
            return Ok(AdjacentFileRead::Unsafe);
        };
        if child_name.components().count() != 1
            || !matches!(child_name.components().next(), Some(Component::Normal(_)))
        {
            return Ok(AdjacentFileRead::Unsafe);
        }
        let directory = if let Some(directory) = media.parent_directory.as_deref() {
            directory.try_clone().map_err(|_| ApiError::Unavailable)?
        } else {
            let parent = media.relative.parent().unwrap_or_else(|| Path::new("."));
            match media.open_relative_dir(parent) {
                Ok(directory) => directory,
                Err(error) if is_unsafe_asset_path(&error) => {
                    return Ok(AdjacentFileRead::Unsafe);
                }
                Err(_) => return Err(ApiError::Unavailable),
            }
        };
        let file = match open_regular_file_no_symlinks(&directory, Path::new(name)) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(AdjacentFileRead::Missing);
            }
            Err(error) if is_unsafe_asset_path(&error) => return Ok(AdjacentFileRead::Unsafe),
            Err(_) => return Err(ApiError::Unavailable),
        };
        read_bounded_open_file(file, max_bytes)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

#[allow(dead_code)] // Used by the adjacent-asset reader added with metadata routes.
fn is_unsafe_asset_path(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
        || matches!(error.raw_os_error(), Some(code) if [libc::ELOOP, libc::ENOTDIR].contains(&code))
}

fn read_bounded_open_file(file: File, max_bytes: usize) -> Result<AdjacentFileRead, ApiError> {
    let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
    if !metadata.is_file() {
        return Ok(AdjacentFileRead::Unsafe);
    }
    if metadata.len() > max_bytes as u64 {
        return Ok(AdjacentFileRead::Unsafe);
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.into_std()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::Unavailable)?;
    if bytes.len() > max_bytes {
        return Ok(AdjacentFileRead::Unsafe);
    }
    Ok(AdjacentFileRead::Content(bytes))
}

pub(super) async fn resolve_under_roots(
    pool: &PgPool,
    item: ItemRecord,
    configured_roots: &[PathBuf],
) -> Result<ResolvedMedia, ApiError> {
    let permit = filesystem_permit()?;
    if configured_roots.len() > 64 {
        return Err(ApiError::Unavailable);
    }
    let mut roots = Vec::with_capacity(configured_roots.len());
    for configured_root in configured_roots {
        let Some((device, inode)) =
            crate::db::library_root_identity(pool, item.library_id, configured_root).await?
        else {
            continue;
        };
        roots.push((configured_root.clone(), device, inode));
    }
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        resolve_with_root_identities(item, roots)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

pub(super) async fn resolve_directory_under_roots(
    pool: &PgPool,
    item: ItemRecord,
    configured_roots: &[PathBuf],
) -> Result<ResolvedDirectory, ApiError> {
    let permit = filesystem_permit()?;
    if configured_roots.len() > 64 {
        return Err(ApiError::Unavailable);
    }
    let mut roots = Vec::with_capacity(configured_roots.len());
    for configured_root in configured_roots {
        let Some((device, inode)) =
            crate::db::library_root_identity(pool, item.library_id, configured_root).await?
        else {
            continue;
        };
        roots.push((configured_root.clone(), device, inode));
    }
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        resolve_directory_with_root_identities(item, roots)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

fn resolve_directory_with_root_identities(
    item: ItemRecord,
    roots: Vec<(PathBuf, u64, u64)>,
) -> Result<ResolvedDirectory, ApiError> {
    for (canonical_root, expected_device, expected_inode) in roots {
        let Ok(relative) = item.path.strip_prefix(&canonical_root) else {
            continue;
        };
        if !safe_relative_directory_path(relative) {
            continue;
        }
        let mut root_options = StdOpenOptions::new();
        root_options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let Ok(root_file) = root_options.open(&canonical_root) else {
            continue;
        };
        let Ok(root_metadata) = root_file.metadata() else {
            continue;
        };
        if !root_metadata.is_dir()
            || root_metadata.dev() != expected_device
            || root_metadata.ino() != expected_inode
        {
            continue;
        }
        let root = Arc::new(Dir::from_std_file(root_file));
        let relative = relative.to_path_buf();
        let Ok(directory) = open_directory_no_symlinks(&root, &relative) else {
            continue;
        };
        // Do not bind folder asset access to the directory mtime: adding a
        // poster or NFO changes that mtime by design and is a normal library
        // workflow. The held descriptor, registered-root identity, and
        // no-symlink traversal are the trust boundary for these child assets.
        return Ok(ResolvedDirectory {
            directory: Arc::new(directory),
        });
    }
    Err(ApiError::NotFound)
}

fn resolve_with_root_identities(
    item: ItemRecord,
    roots: Vec<(PathBuf, u64, u64)>,
) -> Result<ResolvedMedia, ApiError> {
    for (canonical_root, expected_device, expected_inode) in roots {
        let Ok(relative) = item.path.strip_prefix(&canonical_root) else {
            continue;
        };
        if !safe_relative_path(relative) {
            continue;
        }
        // Open the canonical root without following a replacement symlink,
        // then verify the held descriptor against the scanner's persisted
        // device/inode pair before any catalog-relative read.
        let mut root_options = StdOpenOptions::new();
        root_options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let Ok(root_file) = root_options.open(&canonical_root) else {
            continue;
        };
        let Ok(root_metadata) = root_file.metadata() else {
            continue;
        };
        if !root_metadata.is_dir()
            || root_metadata.dev() != expected_device
            || root_metadata.ino() != expected_inode
        {
            continue;
        }
        let root = Arc::new(Dir::from_std_file(root_file));
        let relative = relative.to_path_buf();
        let parent = relative.parent().unwrap_or_else(|| Path::new("."));
        let Some(name) = relative.file_name() else {
            continue;
        };
        let Ok(parent_directory) = open_directory_no_symlinks(&root, parent) else {
            continue;
        };
        // Opening a catalog path may block on remote storage. The bounded
        // resolver pool contains this work; O_NONBLOCK avoids FIFO hangs.
        let Ok(file) = open_regular_file_no_symlinks(&parent_directory, Path::new(name)) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata_matches_known_catalog_fields(&item, &metadata) {
            continue;
        }
        let catalog_identity_matches = metadata_matches_catalog(&item, &metadata);
        return Ok(ResolvedMedia {
            absolute_path: item.path.clone(),
            item,
            root,
            parent_directory: Some(Arc::new(parent_directory)),
            relative,
            catalog_identity_matches,
        });
    }
    Err(ApiError::NotFound)
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn safe_relative_directory_path(path: &Path) -> bool {
    path.as_os_str().is_empty()
        || path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(all(test, unix))]
mod tests {
    use super::{resolve_directory_with_root_identities, resolve_with_root_identities};
    use crate::{ApiError, library::ItemRecord};
    use chrono::Utc;
    use std::{
        fs,
        os::unix::fs::MetadataExt,
        path::{Path, PathBuf},
        time::Duration,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn path_resolution_rejects_symlink_escape_and_special_files() {
        let base = std::env::temp_dir().join(format!("puffinbox-path-{}", Uuid::new_v4()));
        let root = base.join("library");
        fs::create_dir_all(&root).unwrap();
        let outside = base.join("outside.mp4");
        fs::write(&outside, b"private").unwrap();
        let escaped = root.join("escape.mp4");
        std::os::unix::fs::symlink(&outside, &escaped).unwrap();

        let escaped_item = item_for(&escaped);
        assert!(matches!(
            resolve_test(escaped_item, &root),
            Err(ApiError::NotFound)
        ));

        let hidden = root.join("hidden.mp4");
        fs::write(&hidden, b"uncatalogued bytes").unwrap();
        let in_root_alias = root.join("alias.mp4");
        std::os::unix::fs::symlink(&hidden, &in_root_alias).unwrap();
        assert!(matches!(
            resolve_test(item_for(&in_root_alias), &root),
            Err(ApiError::NotFound)
        ));

        let hidden_dir = root.join("hidden-dir");
        fs::create_dir(&hidden_dir).unwrap();
        let hidden_nested_file = hidden_dir.join("nested.mp4");
        fs::write(&hidden_nested_file, b"uncatalogued nested bytes").unwrap();
        let directory_alias = root.join("alias-dir");
        std::os::unix::fs::symlink(&hidden_dir, &directory_alias).unwrap();
        assert!(matches!(
            resolve_test(item_for(&directory_alias.join("nested.mp4")), &root),
            Err(ApiError::NotFound)
        ));

        let fifo = root.join("blocking.mp4");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let fifo_result = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || resolve_test(item_for(&fifo), &root)),
        )
        .await;
        assert!(
            matches!(fifo_result, Ok(Ok(Err(ApiError::NotFound)))),
            "FIFO inputs must fail promptly and never reach media tools"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn path_resolution_opens_regular_files_relative_to_the_library_capability() {
        let root = std::env::temp_dir().join(format!("puffinbox-path-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.mp4");
        fs::write(&path, b"fixture").unwrap();
        let resolved = resolve_test(item_for(&path), &root).unwrap();
        assert_eq!(resolved.relative, PathBuf::from("sample.mp4"));
        assert!(resolved.open_file().unwrap().metadata().unwrap().is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cached_metadata_requires_the_opened_file_identity_to_match_catalog() {
        let root = std::env::temp_dir().join(format!("puffinbox-path-identity-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.mp4");
        fs::write(&path, b"fixture").unwrap();
        let standard_metadata = fs::metadata(&path).unwrap();
        let modified = chrono::DateTime::<chrono::Utc>::from(standard_metadata.modified().unwrap());
        let mut item = item_for(&path);
        item.size_bytes = Some(standard_metadata.len() as i64);
        item.date_modified = Some(modified);
        let resolved = resolve_test(item, &root).unwrap();
        assert!(resolved.catalog_identity_matches);

        fs::write(&path, b"changed bytes with another size").unwrap();
        assert!(matches!(
            resolved.open_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn adjacent_asset_read_is_bounded_and_does_not_follow_symlinks() {
        let base = std::env::temp_dir().join(format!("puffinbox-adjacent-{}", Uuid::new_v4()));
        let root = base.join("library");
        fs::create_dir_all(&root).unwrap();
        let media_path = root.join("movie.mkv");
        fs::write(&media_path, b"video").unwrap();
        let nfo_path = root.join("movie.nfo");
        fs::write(&nfo_path, b"<movie/>\n").unwrap();
        let outside = base.join("outside.jpg");
        fs::write(&outside, b"outside asset").unwrap();
        let link = root.join("movie-poster.jpg");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let media = resolve_test(item_for(&media_path), &root).unwrap();

        assert_eq!(
            super::read_relative_bounded(media.clone(), PathBuf::from("movie.nfo"), 32)
                .await
                .unwrap(),
            super::AdjacentFileRead::Content(b"<movie/>\n".to_vec())
        );
        assert_eq!(
            super::read_relative_bounded(media.clone(), PathBuf::from("missing.nfo"), 32)
                .await
                .unwrap(),
            super::AdjacentFileRead::Missing,
            "only a missing leaf inside an opened catalog directory is classified as Missing"
        );
        assert_eq!(
            super::read_relative_bounded(media.clone(), PathBuf::from("movie.nfo"), 4)
                .await
                .unwrap(),
            super::AdjacentFileRead::Unsafe,
            "oversized assets are rejected as invalid metadata rather than retried as storage failures"
        );
        assert_eq!(
            super::read_relative_bounded(media.clone(), PathBuf::from("movie-poster.jpg"), 64)
                .await
                .unwrap(),
            super::AdjacentFileRead::Unsafe,
            "sibling symlinks must not be read"
        );
        assert!(matches!(
            super::read_relative_bounded(media, PathBuf::from("missing-directory/movie.nfo"), 32)
                .await,
            Err(ApiError::Unavailable)
        ));
        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn directory_asset_reader_supports_series_children_without_following_links() {
        let base = std::env::temp_dir().join(format!("puffinbox-dir-assets-{}", Uuid::new_v4()));
        let root = base.join("library");
        let series = root.join("Series");
        fs::create_dir_all(&series).unwrap();
        fs::write(series.join("tvshow.nfo"), b"<tvshow/>\n").unwrap();
        fs::write(series.join("poster.jpg"), b"cover-bytes").unwrap();
        let hidden = base.join("hidden.nfo");
        fs::write(&hidden, b"must not be read").unwrap();
        std::os::unix::fs::symlink(&hidden, series.join("unsafe.nfo")).unwrap();
        let root_meta = fs::metadata(&root).unwrap();
        let directory = resolve_directory_with_root_identities(
            folder_item_for(&series),
            vec![(
                fs::canonicalize(&root).unwrap(),
                root_meta.dev(),
                root_meta.ino(),
            )],
        )
        .unwrap();

        assert_eq!(
            super::read_directory_child_bounded(directory, PathBuf::from("tvshow.nfo"), 64,)
                .await
                .unwrap(),
            super::AdjacentFileRead::Content(b"<tvshow/>\n".to_vec())
        );
        let directory = resolve_directory_with_root_identities(
            folder_item_for(&series),
            vec![(
                fs::canonicalize(&root).unwrap(),
                root_meta.dev(),
                root_meta.ino(),
            )],
        )
        .unwrap();
        assert_eq!(
            super::read_directory_child_bounded(directory, PathBuf::from("missing.nfo"), 64,)
                .await
                .unwrap(),
            super::AdjacentFileRead::Missing
        );
        let directory = resolve_directory_with_root_identities(
            folder_item_for(&series),
            vec![(
                fs::canonicalize(&root).unwrap(),
                root_meta.dev(),
                root_meta.ino(),
            )],
        )
        .unwrap();
        assert_eq!(
            super::read_directory_child_bounded(directory, PathBuf::from("unsafe.nfo"), 64,)
                .await
                .unwrap(),
            super::AdjacentFileRead::Unsafe
        );

        let alias = root.join("SeriesAlias");
        std::os::unix::fs::symlink(&series, &alias).unwrap();
        assert!(matches!(
            resolve_directory_with_root_identities(
                folder_item_for(&alias),
                vec![(
                    fs::canonicalize(&root).unwrap(),
                    root_meta.dev(),
                    root_meta.ino()
                )],
            ),
            Err(ApiError::NotFound)
        ));
        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn file_sidecar_reader_keeps_the_resolved_parent_directory_descriptor() {
        let base = std::env::temp_dir().join(format!("puffinbox-adjacent-swap-{}", Uuid::new_v4()));
        let root = base.join("library");
        fs::create_dir_all(&root).unwrap();
        let folder = root.join("Movie");
        fs::create_dir(&folder).unwrap();
        let media_path = folder.join("movie.mkv");
        fs::write(&media_path, b"video").unwrap();
        fs::write(folder.join("movie.nfo"), b"cataloged parent").unwrap();
        let media = resolve_test(item_for(&media_path), &root).unwrap();

        let moved = root.join("Movie-old");
        fs::rename(&folder, &moved).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("movie.nfo"), b"replacement parent secret").unwrap();

        assert_eq!(
            super::read_adjacent_bounded(media, PathBuf::from("movie.nfo"), 64)
                .await
                .unwrap(),
            super::AdjacentFileRead::Content(b"cataloged parent".to_vec()),
            "sidecar reads must stay attached to the directory resolved with the catalog item"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn folder_asset_added_after_scan_is_still_readable() {
        let base = std::env::temp_dir().join(format!("puffinbox-dir-asset-new-{}", Uuid::new_v4()));
        let root = base.join("library");
        let series = root.join("Series");
        fs::create_dir_all(&series).unwrap();
        let mut item = folder_item_for(&series);
        item.date_modified = Some(chrono::DateTime::<chrono::Utc>::from(
            fs::metadata(&series).unwrap().modified().unwrap(),
        ));
        let root_meta = fs::metadata(&root).unwrap();
        fs::write(series.join("tvshow.nfo"), b"added after scan").unwrap();

        let directory = resolve_directory_with_root_identities(
            item,
            vec![(
                fs::canonicalize(&root).unwrap(),
                root_meta.dev(),
                root_meta.ino(),
            )],
        )
        .unwrap();
        assert_eq!(
            super::read_directory_child_bounded(directory, PathBuf::from("tvshow.nfo"), 64,)
                .await
                .unwrap(),
            super::AdjacentFileRead::Content(b"added after scan".to_vec())
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn root_replacement_is_rejected_by_the_persisted_directory_identity() {
        let base = std::env::temp_dir().join(format!("puffinbox-root-swap-{}", Uuid::new_v4()));
        let root = base.join("library");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.mp4");
        fs::write(&path, b"original").unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let original_root_metadata = fs::metadata(&canonical_root).unwrap();
        let identity = (
            canonical_root,
            original_root_metadata.dev(),
            original_root_metadata.ino(),
        );
        let item = item_for(&path);

        let moved = base.join("original-root");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("sample.mp4"), b"replacement-secret").unwrap();
        assert!(matches!(
            resolve_with_root_identities(item, vec![identity]),
            Err(ApiError::NotFound)
        ));
        let _ = fs::remove_dir_all(base);
    }

    fn resolve_test(item: ItemRecord, root: &Path) -> Result<super::ResolvedMedia, ApiError> {
        let canonical_root = fs::canonicalize(root).map_err(|_| ApiError::NotFound)?;
        let metadata = fs::metadata(&canonical_root).map_err(|_| ApiError::NotFound)?;
        resolve_with_root_identities(item, vec![(canonical_root, metadata.dev(), metadata.ino())])
    }

    fn item_for(path: &Path) -> ItemRecord {
        ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "fixture.mp4".to_owned(),
            sort_name: "fixture".to_owned(),
            item_type: "Movie".to_owned(),
            path: path.to_path_buf(),
            container: Some("mp4".to_owned()),
            size_bytes: None,
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::json!({}),
        }
    }

    fn folder_item_for(path: &Path) -> ItemRecord {
        let mut item = item_for(path);
        item.item_type = "Series".to_owned();
        item.name = "Series".to_owned();
        item.container = None;
        item.size_bytes = None;
        item
    }
}
