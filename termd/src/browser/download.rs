use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::file_offer::{
    FILE_OFFER_TTL_MS, FileIdentity, FileOfferError, InspectedFileOffer, inspect_file_offer_exact,
};

const PARTIAL_DOWNLOAD_SUFFIX: &str = ".crdownload";
const CHROMIUM_TEMP_PREFIXES: [&str; 2] = [".org.chromium.Chromium.", ".com.google.Chrome."];
const BROWSER_DOWNLOAD_RETENTION: Duration = Duration::from_secs(FILE_OFFER_TTL_MS / 1_000);
pub(super) const BROWSER_DOWNLOAD_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const BROWSER_DOWNLOAD_SESSION_MAX_FILES: usize = 128;
const BROWSER_DOWNLOAD_TOTAL_MAX_FILES: usize = 512;
const BROWSER_DOWNLOAD_TOTAL_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct DownloadPolicy {
    retention: Duration,
    session_max_files: usize,
    session_max_bytes: u64,
    total_max_files: usize,
    total_max_bytes: u64,
}

const DOWNLOAD_POLICY: DownloadPolicy = DownloadPolicy {
    retention: BROWSER_DOWNLOAD_RETENTION,
    session_max_files: BROWSER_DOWNLOAD_SESSION_MAX_FILES,
    session_max_bytes: BROWSER_DOWNLOAD_FILE_MAX_BYTES,
    total_max_files: BROWSER_DOWNLOAD_TOTAL_MAX_FILES,
    total_max_bytes: BROWSER_DOWNLOAD_TOTAL_MAX_BYTES,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BrowserDownloadCandidate {
    path: PathBuf,
    identity: FileIdentity,
}

impl BrowserDownloadCandidate {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn inspect(&self) -> Result<InspectedFileOffer, FileOfferError> {
        inspect_file_offer_exact(&self.path, self.identity)
    }
}

#[derive(Debug, Clone)]
struct DownloadDiskEntry {
    candidate: BrowserDownloadCandidate,
    completed: bool,
}

impl PartialEq for DownloadDiskEntry {
    fn eq(&self, other: &Self) -> bool {
        self.candidate == other.candidate
    }
}

impl Eq for DownloadDiskEntry {}

impl PartialOrd for DownloadDiskEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DownloadDiskEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.candidate
            .identity
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .cmp(&other.candidate.identity.modified().unwrap_or(UNIX_EPOCH))
            .then_with(|| self.candidate.path.cmp(&other.candidate.path))
            .then_with(|| self.candidate.identity.cmp(&other.candidate.identity))
    }
}

#[derive(Debug, Default)]
pub(super) struct BrowserDownloadTracker {
    pending: HashMap<PathBuf, FileIdentity>,
    handled: HashSet<BrowserDownloadCandidate>,
    inspected: HashMap<BrowserDownloadCandidate, InspectedFileOffer>,
}

impl BrowserDownloadTracker {
    pub(super) fn at_startup(existing: Vec<BrowserDownloadCandidate>) -> Self {
        Self {
            handled: existing.into_iter().collect(),
            ..Self::default()
        }
    }

    pub(super) fn observe(
        &mut self,
        downloads: Vec<BrowserDownloadCandidate>,
    ) -> Vec<BrowserDownloadCandidate> {
        let current = downloads.iter().cloned().collect::<HashSet<_>>();
        self.pending.retain(|path, identity| {
            current.contains(&BrowserDownloadCandidate {
                path: path.clone(),
                identity: *identity,
            })
        });
        self.handled.retain(|candidate| current.contains(candidate));
        self.inspected
            .retain(|candidate, _| current.contains(candidate));

        downloads
            .into_iter()
            .filter(|candidate| !self.handled.contains(candidate))
            .filter_map(|candidate| {
                let stable = self.pending.get(&candidate.path) == Some(&candidate.identity);
                self.pending
                    .insert(candidate.path.clone(), candidate.identity);
                stable.then_some(candidate)
            })
            .collect()
    }

    pub(super) fn cached_inspection(
        &self,
        candidate: &BrowserDownloadCandidate,
    ) -> Option<InspectedFileOffer> {
        self.inspected.get(candidate).cloned()
    }

    pub(super) fn cache_inspection(
        &mut self,
        candidate: BrowserDownloadCandidate,
        inspected: InspectedFileOffer,
    ) {
        self.inspected.insert(candidate, inspected);
    }

    pub(super) fn mark_handled(&mut self, candidate: BrowserDownloadCandidate) {
        self.pending.remove(&candidate.path);
        self.inspected.remove(&candidate);
        self.handled.insert(candidate);
    }
}

pub(super) fn scan_downloads(
    root: &Path,
    active_sessions: &HashSet<uuid::Uuid>,
) -> std::io::Result<Vec<BrowserDownloadCandidate>> {
    scan_downloads_with_policy(root, active_sessions, DOWNLOAD_POLICY, SystemTime::now())
}

fn scan_downloads_with_policy(
    root: &Path,
    active_sessions: &HashSet<uuid::Uuid>,
    policy: DownloadPolicy,
    now: SystemTime,
) -> std::io::Result<Vec<BrowserDownloadCandidate>> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "browser download root is not a directory",
        ));
    }

    let mut retained = BinaryHeap::<Reverse<DownloadDiskEntry>>::new();
    let mut retained_bytes = 0_u64;
    for directory in fs::read_dir(root)? {
        let Ok(directory) = directory else {
            continue;
        };
        let Ok(session_id) = uuid::Uuid::parse_str(&directory.file_name().to_string_lossy()) else {
            continue;
        };
        let directory_path = directory.path();
        let Ok(directory_metadata) = fs::symlink_metadata(&directory_path) else {
            continue;
        };
        if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(children) = fs::read_dir(&directory_path) else {
            continue;
        };
        let mut session_retained = BinaryHeap::<Reverse<DownloadDiskEntry>>::new();
        let mut session_retained_bytes = 0_u64;
        for child in children {
            let Ok(child) = child else {
                continue;
            };
            let name = child.file_name();
            let name = name.to_string_lossy();
            if name.is_empty() {
                continue;
            }
            let incomplete = is_incomplete_download_name(&name);
            let path = child.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if incomplete && !active_sessions.contains(&session_id) {
                remove_incomplete_path(&path, &metadata)?;
                continue;
            }
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let identity = FileIdentity::from_metadata(&metadata);
            if !incomplete && is_expired(identity, now, policy.retention) {
                remove_regular_file(&path, identity)?;
                continue;
            }
            retain_newest(
                &mut session_retained,
                &mut session_retained_bytes,
                DownloadDiskEntry {
                    candidate: BrowserDownloadCandidate { path, identity },
                    completed: !incomplete,
                },
                policy.session_max_files,
                policy.session_max_bytes,
            )?;
        }
        for Reverse(entry) in session_retained {
            retain_newest(
                &mut retained,
                &mut retained_bytes,
                entry,
                policy.total_max_files,
                policy.total_max_bytes,
            )?;
        }
    }

    cleanup_empty_closed_download_dirs(root, active_sessions)?;
    let mut retained = retained
        .into_iter()
        .map(|Reverse(entry)| entry)
        .collect::<Vec<_>>();
    retained.sort_by(newest_first);
    Ok(retained
        .into_iter()
        .filter_map(|entry| entry.completed.then_some(entry.candidate))
        .collect())
}

fn retain_newest(
    retained: &mut BinaryHeap<Reverse<DownloadDiskEntry>>,
    retained_bytes: &mut u64,
    entry: DownloadDiskEntry,
    max_files: usize,
    max_bytes: u64,
) -> std::io::Result<()> {
    *retained_bytes = retained_bytes.saturating_add(entry.candidate.identity.len());
    retained.push(Reverse(entry));
    while retained.len() > max_files || *retained_bytes > max_bytes {
        let Reverse(removed) = retained.pop().expect("retained download heap is not empty");
        *retained_bytes = retained_bytes.saturating_sub(removed.candidate.identity.len());
        remove_regular_file(&removed.candidate.path, removed.candidate.identity)?;
    }
    Ok(())
}

fn newest_first(left: &DownloadDiskEntry, right: &DownloadDiskEntry) -> Ordering {
    right.cmp(left)
}

fn cleanup_empty_closed_download_dirs(
    root: &Path,
    active_sessions: &HashSet<uuid::Uuid>,
) -> std::io::Result<()> {
    for directory in fs::read_dir(root)? {
        let Ok(directory) = directory else {
            continue;
        };
        let Ok(session_id) = uuid::Uuid::parse_str(&directory.file_name().to_string_lossy()) else {
            continue;
        };
        if active_sessions.contains(&session_id) {
            continue;
        }
        let path = directory.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        if fs::read_dir(&path).is_ok_and(|mut children| children.next().is_none()) {
            fs::remove_dir(path)?;
        }
    }
    Ok(())
}

fn is_expired(identity: FileIdentity, now: SystemTime, retention: Duration) -> bool {
    identity
        .modified()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= retention)
}

fn is_incomplete_download_name(name: &str) -> bool {
    name.ends_with(PARTIAL_DOWNLOAD_SUFFIX)
        || CHROMIUM_TEMP_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

pub(super) fn cleanup_incomplete_downloads(directory: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "browser download directory is not a directory",
        ));
    }
    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        if !is_incomplete_download_name(&name.to_string_lossy()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        remove_incomplete_path(&entry.path(), &metadata)?;
    }
    Ok(())
}

fn remove_incomplete_path(path: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    if metadata.is_file() || metadata.file_type().is_symlink() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_regular_file(path: &Path, expected: FileIdentity) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_file()
        && !metadata.file_type().is_symlink()
        && FileIdentity::from_metadata(&metadata) == expected
    {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_download_dir(root: &Path) -> PathBuf {
        let path = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&path).unwrap();
        path
    }

    fn scan(root: &Path) -> Vec<BrowserDownloadCandidate> {
        scan_downloads(root, &HashSet::new()).unwrap()
    }

    #[test]
    fn only_reports_a_completed_file_after_two_stable_scans() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let path = directory.join("report.zip");
        fs::write(&path, b"ready").unwrap();
        let mut tracker = BrowserDownloadTracker::default();

        assert!(tracker.observe(scan(root.path())).is_empty());
        let stable = tracker.observe(scan(root.path()));
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].path(), path);

        tracker.mark_handled(stable[0].clone());
        assert!(tracker.observe(scan(root.path())).is_empty());
    }

    #[test]
    fn ignores_and_cleans_partial_downloads_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let partial = directory.join("report.zip.crdownload");
        let chromium_temp = directory.join(".org.chromium.Chromium.a1b2c3");
        let chrome_temp = directory.join(".com.google.Chrome.d4e5f6");
        fs::write(&partial, b"partial").unwrap();
        fs::write(&chromium_temp, b"partial").unwrap();
        fs::write(&chrome_temp, b"partial").unwrap();
        fs::create_dir(directory.join("nested")).unwrap();
        let target = directory.join("target.txt");
        fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, directory.join("link.txt")).unwrap();

        let downloads = scan(root.path());
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].path(), target);
        assert!(!partial.exists());
        assert!(!chromium_temp.exists());
        assert!(!chrome_temp.exists());
    }

    #[test]
    fn changing_a_file_restarts_the_stability_window() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let path = directory.join("result.bin");
        fs::write(&path, b"first").unwrap();
        let mut tracker = BrowserDownloadTracker::default();

        assert!(tracker.observe(scan(root.path())).is_empty());
        fs::write(&path, b"second-version").unwrap();
        assert!(tracker.observe(scan(root.path())).is_empty());
        assert_eq!(tracker.observe(scan(root.path())).len(), 1);
    }

    #[test]
    fn removing_a_handled_file_allows_a_new_file_at_the_same_path() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let session_id =
            uuid::Uuid::parse_str(directory.file_name().unwrap().to_str().unwrap()).unwrap();
        let active_sessions = HashSet::from([session_id]);
        let scan_active = || scan_downloads(root.path(), &active_sessions).unwrap();
        let path = directory.join("result.bin");
        fs::write(&path, b"first").unwrap();
        let mut tracker = BrowserDownloadTracker::default();
        let _ = tracker.observe(scan_active());
        let candidate = tracker.observe(scan_active()).remove(0);
        tracker.mark_handled(candidate);

        fs::remove_file(&path).unwrap();
        assert!(tracker.observe(scan_active()).is_empty());
        fs::write(&path, b"second").unwrap();
        assert!(tracker.observe(scan_active()).is_empty());
        assert_eq!(tracker.observe(scan_active()).len(), 1);
    }

    #[test]
    fn exact_inspection_rejects_a_path_replaced_after_scanning() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let path = directory.join("result.bin");
        fs::write(&path, b"expected").unwrap();
        let candidate = scan(root.path()).remove(0);
        let replacement = directory.join("replacement.bin");
        fs::write(&replacement, b"not expected").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&replacement, &path).unwrap();

        assert!(candidate.inspect().is_err());
    }

    #[test]
    fn quotas_keep_newest_files_without_fixed_scan_starvation() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let first = directory.join("first.bin");
        let second = directory.join("second.bin");
        let third = directory.join("third.bin");
        fs::write(&first, b"1111").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&second, b"2222").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&third, b"3333").unwrap();
        let policy = DownloadPolicy {
            retention: Duration::from_secs(60),
            session_max_files: 2,
            session_max_bytes: 8,
            total_max_files: 2,
            total_max_bytes: 8,
        };

        let downloads =
            scan_downloads_with_policy(root.path(), &HashSet::new(), policy, SystemTime::now())
                .unwrap();
        assert_eq!(downloads.len(), 2);
        assert!(!first.exists());
        assert!(second.exists());
        assert!(third.exists());
    }

    #[test]
    fn startup_tracker_does_not_reoffer_retained_downloads() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let retained_path = directory.join("retained.bin");
        fs::write(&retained_path, b"already offered").unwrap();
        let mut tracker = BrowserDownloadTracker::at_startup(scan(root.path()));

        assert!(tracker.observe(scan(root.path())).is_empty());
        assert!(tracker.observe(scan(root.path())).is_empty());

        let new_path = directory.join("new.bin");
        fs::write(&new_path, b"new download").unwrap();
        assert!(tracker.observe(scan(root.path())).is_empty());
        let offered = tracker.observe(scan(root.path()));
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].path(), new_path);
    }

    #[test]
    fn cached_inspection_survives_delivery_retries_for_the_same_file() {
        let root = tempfile::tempdir().unwrap();
        let directory = session_download_dir(root.path());
        let path = directory.join("large.bin");
        fs::write(&path, b"hash once").unwrap();
        let candidate = scan(root.path()).remove(0);
        let inspected = candidate.inspect().unwrap();
        let mut tracker = BrowserDownloadTracker::default();

        tracker.cache_inspection(candidate.clone(), inspected);
        assert!(tracker.cached_inspection(&candidate).is_some());
        tracker.mark_handled(candidate.clone());
        assert!(tracker.cached_inspection(&candidate).is_none());
    }
}
