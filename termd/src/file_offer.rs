use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use sha2::{Digest, Sha256};
use termd_proto::{FileOfferPayload, UnixTimestampMillis};
use thiserror::Error;
use uuid::Uuid;

pub const FILE_OFFER_TTL_MS: u64 = 24 * 60 * 60 * 1000;
pub const FILE_OFFER_LIMIT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug)]
struct FileOfferEntry {
    payload: FileOfferPayload,
    canonical_path: PathBuf,
    identity: FileIdentity,
    content_sha256: [u8; 32],
}

#[derive(Debug)]
pub struct InspectedFileOffer {
    name: String,
    canonical_path: PathBuf,
    size_bytes: u64,
    identity: FileIdentity,
    content_sha256: [u8; 32],
}

#[derive(Debug)]
pub struct PreparedFileOffer {
    pub payload: FileOfferPayload,
    pub file: fs::File,
    pub content_sha256: [u8; 32],
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum FileOfferError {
    #[error("offered file was not found")]
    NotFound,
    #[error("offered path is not a regular file")]
    NotRegular,
    #[error("offered file cannot be read")]
    Unreadable,
    #[error("file offer was not found")]
    OfferNotFound,
    #[error("offered file has expired or changed")]
    Invalidated,
    #[error("a connected client is not ready for another file offer")]
    DeliveryBusy,
}

impl FileOfferError {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "file_not_found",
            Self::NotRegular => "file_not_regular",
            Self::Unreadable => "file_unreadable",
            Self::OfferNotFound => "file_offer_not_found",
            Self::Invalidated => "file_offer_invalid",
            Self::DeliveryBusy => "file_offer_delivery_busy",
        }
    }

    pub fn safe_message(self) -> &'static str {
        match self {
            Self::NotFound => "offered file was not found",
            Self::NotRegular => "offered path is not a regular file",
            Self::Unreadable => "offered file cannot be read",
            Self::OfferNotFound => "file offer was not found",
            Self::Invalidated => "offered file has expired or changed",
            Self::DeliveryBusy => "a connected client is not ready for another file offer",
        }
    }
}

#[derive(Debug, Default)]
pub struct FileOfferStore {
    entries: VecDeque<FileOfferEntry>,
}

impl FileOfferStore {
    pub fn create(
        &mut self,
        path: impl AsRef<Path>,
        now_ms: UnixTimestampMillis,
    ) -> Result<FileOfferPayload, FileOfferError> {
        let inspected = inspect_file_offer(path)?;
        Ok(self.register(inspected, now_ms))
    }

    pub fn register(
        &mut self,
        inspected: InspectedFileOffer,
        now_ms: UnixTimestampMillis,
    ) -> FileOfferPayload {
        self.expire(now_ms);
        let payload = FileOfferPayload {
            offer_id: Uuid::new_v4(),
            name: inspected.name,
            path: inspected.canonical_path.to_string_lossy().into_owned(),
            size_bytes: inspected.size_bytes,
            created_at_ms: now_ms,
            expires_at_ms: UnixTimestampMillis(now_ms.0.saturating_add(FILE_OFFER_TTL_MS)),
        };
        if self.entries.len() >= FILE_OFFER_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(FileOfferEntry {
            payload: payload.clone(),
            canonical_path: inspected.canonical_path,
            identity: inspected.identity,
            content_sha256: inspected.content_sha256,
        });
        payload
    }

    pub fn resolve(
        &mut self,
        offer_id: Uuid,
        now_ms: UnixTimestampMillis,
    ) -> Result<FileOfferPayload, FileOfferError> {
        self.prepare(offer_id, now_ms)
            .map(|prepared| prepared.payload)
    }

    pub fn prepare(
        &mut self,
        offer_id: Uuid,
        now_ms: UnixTimestampMillis,
    ) -> Result<PreparedFileOffer, FileOfferError> {
        self.expire(now_ms);
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.payload.offer_id == offer_id)
            .ok_or(FileOfferError::OfferNotFound)?;
        let file = open_regular_file(&entry.canonical_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FileOfferError::Invalidated
            } else {
                FileOfferError::Unreadable
            }
        })?;
        let metadata = file.metadata().map_err(|_| FileOfferError::Invalidated)?;
        if !metadata.is_file() || FileIdentity::from_metadata(&metadata) != entry.identity {
            return Err(FileOfferError::Invalidated);
        }
        let path_metadata =
            fs::metadata(&entry.canonical_path).map_err(|_| FileOfferError::Invalidated)?;
        if FileIdentity::from_metadata(&path_metadata) != entry.identity {
            return Err(FileOfferError::Invalidated);
        }
        Ok(PreparedFileOffer {
            payload: entry.payload.clone(),
            file,
            content_sha256: entry.content_sha256,
        })
    }

    pub fn validate_open_file(
        &mut self,
        payload: &FileOfferPayload,
        file: &fs::File,
        now_ms: UnixTimestampMillis,
    ) -> Result<(), FileOfferError> {
        self.expire(now_ms);
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.payload.offer_id == payload.offer_id)
            .ok_or(FileOfferError::OfferNotFound)?;
        let open_metadata = file.metadata().map_err(|_| FileOfferError::Invalidated)?;
        let path_metadata =
            fs::metadata(&entry.canonical_path).map_err(|_| FileOfferError::Invalidated)?;
        if !open_metadata.is_file()
            || FileIdentity::from_metadata(&open_metadata) != entry.identity
            || FileIdentity::from_metadata(&path_metadata) != entry.identity
        {
            return Err(FileOfferError::Invalidated);
        }
        Ok(())
    }

    fn expire(&mut self, now_ms: UnixTimestampMillis) {
        self.entries
            .retain(|entry| entry.payload.expires_at_ms > now_ms);
    }
}

pub fn inspect_file_offer(path: impl AsRef<Path>) -> Result<InspectedFileOffer, FileOfferError> {
    let canonical_path = fs::canonicalize(path.as_ref()).map_err(map_canonicalize_error)?;
    let mut file = open_regular_file(&canonical_path).map_err(map_open_error)?;
    let initial_metadata = file.metadata().map_err(|_| FileOfferError::Unreadable)?;
    if !initial_metadata.is_file() {
        return Err(FileOfferError::NotRegular);
    }
    let identity = FileIdentity::from_metadata(&initial_metadata);
    let name = canonical_path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or(FileOfferError::NotRegular)?
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| FileOfferError::Unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let final_metadata = file.metadata().map_err(|_| FileOfferError::Invalidated)?;
    let path_metadata = fs::metadata(&canonical_path).map_err(|_| FileOfferError::Invalidated)?;
    if FileIdentity::from_metadata(&final_metadata) != identity
        || FileIdentity::from_metadata(&path_metadata) != identity
    {
        return Err(FileOfferError::Invalidated);
    }
    Ok(InspectedFileOffer {
        name,
        canonical_path,
        size_bytes: initial_metadata.len(),
        identity,
        content_sha256: hasher.finalize().into(),
    })
}

fn open_regular_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path)
}

fn map_canonicalize_error(error: std::io::Error) -> FileOfferError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FileOfferError::NotFound,
        std::io::ErrorKind::PermissionDenied => FileOfferError::Unreadable,
        _ => FileOfferError::Unreadable,
    }
}

fn map_open_error(error: std::io::Error) -> FileOfferError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FileOfferError::NotFound,
        std::io::ErrorKind::PermissionDenied => FileOfferError::Unreadable,
        _ => FileOfferError::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "termd-file-offer-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_resolves_and_expires_regular_file_offer() {
        let dir = TestDir::new("lifecycle");
        let path = dir.0.join("report.zip");
        fs::write(&path, b"report").unwrap();
        let mut store = FileOfferStore::default();
        let offer = store.create(&path, UnixTimestampMillis(10)).unwrap();
        assert_eq!(offer.name, "report.zip");
        assert_eq!(offer.size_bytes, 6);
        assert_eq!(
            store
                .resolve(offer.offer_id, UnixTimestampMillis(11))
                .unwrap(),
            offer
        );
        assert_eq!(
            store.resolve(offer.offer_id, offer.expires_at_ms),
            Err(FileOfferError::OfferNotFound)
        );
    }

    #[test]
    fn rejects_directories_and_accepts_symlinks_to_files() {
        let dir = TestDir::new("types");
        let mut store = FileOfferStore::default();
        assert_eq!(
            store.create(&dir.0, UnixTimestampMillis(1)),
            Err(FileOfferError::NotRegular)
        );
        let target = dir.0.join("target.txt");
        fs::write(&target, b"ok").unwrap();
        #[cfg(unix)]
        {
            let link = dir.0.join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let offer = store.create(link, UnixTimestampMillis(2)).unwrap();
            assert_eq!(offer.path, target.to_string_lossy());

            let fifo = dir.0.join("pipe");
            let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
            assert_eq!(
                store.create(fifo, UnixTimestampMillis(3)),
                Err(FileOfferError::NotRegular)
            );
        }
    }

    #[test]
    fn invalidates_modified_and_replaced_files() {
        let dir = TestDir::new("identity");
        let path = dir.0.join("result.bin");
        fs::write(&path, b"before").unwrap();
        let mut store = FileOfferStore::default();
        let modified = store.create(&path, UnixTimestampMillis(1)).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"after").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            store.resolve(modified.offer_id, UnixTimestampMillis(2)),
            Err(FileOfferError::Invalidated)
        );

        fs::write(&path, b"original").unwrap();
        let replaced = store.create(&path, UnixTimestampMillis(3)).unwrap();
        let replacement = dir.0.join("replacement.bin");
        fs::write(&replacement, b"original").unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            store.resolve(replaced.offer_id, UnixTimestampMillis(4)),
            Err(FileOfferError::Invalidated)
        );
    }

    #[test]
    fn evicts_the_oldest_offer_at_the_bound() {
        let dir = TestDir::new("limit");
        let path = dir.0.join("item");
        fs::write(&path, b"x").unwrap();
        let mut store = FileOfferStore::default();
        let first = store.create(&path, UnixTimestampMillis(1)).unwrap();
        for now in 2..=(FILE_OFFER_LIMIT as u64 + 1) {
            store.create(&path, UnixTimestampMillis(now)).unwrap();
        }
        assert_eq!(
            store.resolve(first.offer_id, UnixTimestampMillis(100)),
            Err(FileOfferError::OfferNotFound)
        );
    }
}
