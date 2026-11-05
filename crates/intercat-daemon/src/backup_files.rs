//! Bounded, descriptor-relative file comparison. File bytes never enter the journal.
use crate::Result;
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
};

const MAX_FILES: usize = 50_000;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 1024 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    pub kind: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub op_id: uuid::Uuid,
    pub track_id: String,
    pub baseline: String,
    pub seal: String,
    pub files: usize,
    pub directories: usize,
    pub changes: Vec<Change>,
    pub protected: Vec<String>,
    pub unsupported: Vec<String>,
    pub attachments: crate::backup_attachments::Review,
    pub complete: bool,
    pub stale: bool,
}
#[derive(Clone, PartialEq, Eq, Serialize)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: i64,
    mode: u32,
    links: u64,
    uid: u32,
    modified: i64,
    modified_ns: i64,
    changed: i64,
    changed_ns: i64,
}
impl Stamp {
    fn new(s: &libc::stat) -> Self {
        Self {
            dev: s.st_dev as u64,
            ino: s.st_ino,
            len: s.st_size,
            mode: s.st_mode as u32,
            links: s.st_nlink as u64,
            uid: s.st_uid,
            modified: s.st_mtime,
            modified_ns: s.st_mtime_nsec,
            changed: s.st_ctime,
            changed_ns: s.st_ctime_nsec,
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Entry {
    stamp: Stamp,
    digest: Option<Vec<u8>>,
    blocked: Option<String>,
}
impl Entry {
    pub(crate) fn owned_directory(&self) -> bool {
        self.stamp.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
            && self.stamp.mode & 0o077 == 0
            && self.stamp.uid == unsafe { libc::geteuid() }
            && self.blocked.is_none()
    }
    pub(crate) fn matches(&self, digest: &[u8]) -> bool {
        self.stamp.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
            && self.stamp.mode & 0o177 == 0
            && self.stamp.links == 1
            && self.stamp.uid == unsafe { libc::geteuid() }
            && self.blocked.is_none()
            && self.digest.as_deref() == Some(digest)
    }
}
fn parent(root: &File, path: &str) -> Result<(File, CString)> {
    let parts = Path::new(path).components().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 128 {
        return Err("Invalid file review path.".into());
    }
    let mut directory = root.try_clone()?;
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("Invalid file review path.".into());
        };
        let name = CString::new(name.as_bytes())?;
        if index == parts.len() - 1 {
            return Ok((directory, name));
        }
        // The directory descriptor pins every ancestor; a symlink swap cannot
        // redirect a later read outside the reviewed root.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    unreachable!()
}
fn stat(directory: &File, name: &CString) -> Result<libc::stat> {
    let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { value.assume_init() })
}
fn protected(path: &str) -> bool {
    path.split('/').any(intercat_git::secret_path)
}
fn read_entry(root: &File, path: &str, total: &mut u64) -> Result<Entry> {
    let (directory, name) = parent(root, path)?;
    let before = stat(&directory, &name)?;
    let stamp = Stamp::new(&before);
    let kind = before.st_mode & libc::S_IFMT;
    if protected(path) {
        return Ok(Entry {
            stamp,
            digest: None,
            blocked: Some("protected".into()),
        });
    }
    if kind == libc::S_IFDIR {
        return Ok(Entry {
            stamp,
            digest: None,
            blocked: None,
        });
    }
    if !matches!(kind, libc::S_IFREG | libc::S_IFLNK) || before.st_nlink > 1 {
        return Ok(Entry {
            stamp,
            digest: None,
            blocked: Some("unsupported".into()),
        });
    }
    if before.st_size < 0 || before.st_size as u64 > MAX_FILE {
        return Ok(Entry {
            stamp,
            digest: None,
            blocked: Some("unsupported".into()),
        });
    }
    let mut hash = Sha256::new();
    if kind == libc::S_IFLNK {
        let mut bytes = vec![0u8; 65536];
        let count = unsafe {
            libc::readlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if count < 0 || count as usize == bytes.len() {
            return Err("A link changed during file review.".into());
        }
        *total += count as u64;
        if *total > MAX_TOTAL {
            return Err("The file review exceeds its byte limit.".into());
        }
        hash.update(&bytes[..count as usize]);
    } else {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), opened.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if Stamp::new(&unsafe { opened.assume_init() }) != stamp {
            return Err("A file changed during review.".into());
        }
        let mut bytes = [0u8; 65536];
        let mut read = 0u64;
        loop {
            let count = file.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            read += count as u64;
            *total += count as u64;
            if read > MAX_FILE || *total > MAX_TOTAL {
                return Err("The file review exceeds its byte limit.".into());
            }
            hash.update(&bytes[..count]);
        }
        if read != before.st_size as u64 {
            return Err("A file changed during review.".into());
        }
    }
    if Stamp::new(&stat(&directory, &name)?) != stamp {
        return Err("A file changed during review.".into());
    }
    Ok(Entry {
        stamp,
        digest: Some(hash.finalize().to_vec()),
        blocked: None,
    })
}
fn scan(path: &Path) -> Result<BTreeMap<String, Entry>> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut root_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(root.as_raw_fd(), root_stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let root_stamp = Stamp::new(&unsafe { root_stat.assume_init() });
    let mut result = BTreeMap::new();
    let mut skipped = BTreeSet::new();
    let mut total = 0;
    let visited = std::cell::Cell::new(0usize);
    let walker = walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            visited.set(visited.get() + 1);
            if visited.get() > MAX_FILES {
                return true;
            }
            if entry.depth() == 1 && entry.file_name() == ".git" {
                return false;
            }
            let relative = entry.path().strip_prefix(path).unwrap();
            let Some(relative) = relative.to_str() else {
                return true;
            };
            if (entry.file_name() == ".git" || protected(relative)) && entry.file_type().is_dir() {
                skipped.insert(relative.to_owned());
                false
            } else {
                true
            }
        });
    for entry in walker {
        let entry = entry?;
        if entry.depth() == 0 {
            continue;
        }
        if visited.get() > MAX_FILES {
            return Err("Too many files to review at once.".into());
        }
        let relative = entry
            .path()
            .strip_prefix(path)?
            .to_str()
            .ok_or("A file review path is not UTF-8.")?
            .to_owned();
        let value = read_entry(&root, &relative, &mut total)?;
        result.insert(relative, value);
        if result.len() > MAX_FILES {
            return Err("Too many files to review at once.".into());
        }
    }
    for relative in skipped {
        let mut value = read_entry(&root, &relative, &mut total)?;
        value.blocked = Some(
            if protected(&relative) {
                "protected"
            } else {
                "unsupported"
            }
            .into(),
        );
        result.insert(relative, value);
    }
    for (relative, entry) in &result {
        let (directory, name) = parent(&root, relative)?;
        if Stamp::new(&stat(&directory, &name)?) != entry.stamp {
            return Err("Files changed during review. Stop writers and review again.".into());
        }
    }
    let mut final_root = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(root.as_raw_fd(), final_root.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if Stamp::new(&unsafe { final_root.assume_init() }) != root_stamp {
        return Err("The reviewed workspace directory changed.".into());
    }
    let current = std::fs::symlink_metadata(path)?;
    if !current.is_dir() || current.dev() != root_stamp.dev || current.ino() != root_stamp.ino {
        return Err("The reviewed workspace directory changed.".into());
    }
    result.insert(
        String::new(),
        Entry {
            stamp: root_stamp,
            digest: None,
            blocked: None,
        },
    );
    Ok(result)
}
pub struct Inspection {
    pub material: Vec<u8>,
    pub review: Review,
}
pub fn inspect(
    path: &Path,
    baseline: &str,
    op_id: uuid::Uuid,
    track_id: String,
    source: &crate::backup_attachments::Source,
) -> Result<Inspection> {
    if path.canonicalize()? != path {
        return Err("The reviewed workspace path changed.".into());
    }
    let repository = Repository::open(path)?;
    let saved = repository.saved_files(baseline.parse()?)?;
    let attachments = intercat_git::AttachmentPaths::from_paths(saved.keys().map(String::as_str))
        .with_roots(&source.roots)?;
    let before = scan(path)?;
    let originals = crate::backup_attachments::originals(&source.store, &source.records)?;
    let after = scan(path)?;
    if before != after {
        return Err("Files changed during review. Stop writers and review again.".into());
    }
    let (attachment_review, original_material) = crate::backup_attachments::compare(
        originals,
        &after,
        &attachments,
        &source.roots,
        &source.required,
    );
    let mut review = Review {
        op_id,
        track_id,
        baseline: baseline.into(),
        seal: String::new(),
        files: 0,
        directories: 0,
        changes: vec![],
        protected: vec![],
        unsupported: vec![],
        attachments: attachment_review,
        complete: true,
        stale: false,
    };
    let mut baseline_bytes = 0u64;
    for (name, entry) in &after {
        if attachments.excludes(name) {
            continue;
        }
        let is_directory = entry.stamp.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32;
        if is_directory {
            review.directories += 1;
            if saved.contains_key(name) {
                review.changes.push(Change {
                    path: name.clone(),
                    kind: "type-changed".into(),
                });
            }
        }
        if let Some(reason) = &entry.blocked {
            if reason == "protected" {
                review.protected.push(name.clone());
            } else {
                review.unsupported.push(name.clone());
            }
            review.complete = false;
            continue;
        }
        let Some(digest) = &entry.digest else {
            continue;
        };
        review.files += 1;
        let kind = if let Some(file) = saved.get(name) {
            let size = repository.saved_size(file)?;
            baseline_bytes = baseline_bytes
                .checked_add(size)
                .ok_or("The saved files exceed the review limit.")?;
            if baseline_bytes > MAX_TOTAL {
                return Err("The saved files exceed the review limit.".into());
            }
            if size > MAX_FILE {
                review.unsupported.push(name.clone());
                review.complete = false;
                continue;
            }
            let bytes = repository.saved_bytes(file)?;
            let mode = if entry.stamp.mode & libc::S_IFMT as u32 == libc::S_IFLNK as u32 {
                0o120000
            } else if entry.stamp.mode & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            };
            if file.mode as u32 & 0o170000 != mode & 0o170000 {
                Some("type-changed")
            } else if Sha256::digest(bytes).as_slice() != digest {
                Some("modified")
            } else if file.mode as u32 != mode {
                Some("mode-changed")
            } else {
                None
            }
        } else {
            Some("added")
        };
        if let Some(kind) = kind {
            review.changes.push(Change {
                path: name.clone(),
                kind: kind.into(),
            });
        }
    }
    for name in saved.keys().filter(|name| !after.contains_key(*name)) {
        if protected(name) {
            review.protected.push(name.clone());
            review.complete = false;
        } else {
            review.changes.push(Change {
                path: name.clone(),
                kind: "deleted".into(),
            });
        }
    }
    review.changes.sort_by(|a, b| a.path.cmp(&b.path));
    review.directories = review.directories.saturating_sub(1);
    // Reserve the 64 hexadecimal bytes of the keyed seal filled by the caller.
    if serde_json::to_vec(&review)?.len() + 64 > 256 * 1024 {
        return Err("The file review report is too large to display safely.".into());
    }
    // Raw digests remain private input to the keyed seal and are never stored.
    Ok(Inspection {
        material: serde_json::to_vec(&(path, baseline, after, original_material))?,
        review,
    })
}
