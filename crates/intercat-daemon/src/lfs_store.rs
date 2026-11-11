//! Content-checked LFS objects. Descriptor-relative access never follows an
//! object path through a symlink. A published object never overwrites a name.
use crate::Result;
use intercat_git::lfs::Entry;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Seek},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Component, Path},
};
use uuid::Uuid;

const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Copy only objects referenced by the chosen tree. No historical object or
/// alternate LFS store is consulted, and an absent source is never created.
pub fn transfer(source: &Path, target: &Path, entries: &[Entry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let target = Store::create(target)?;
    let mut needed = Vec::new();
    for entry in entries {
        match target.verified(entry) {
            Ok(Some(file)) => {
                full_sync(&file)?;
                target.sync_directories(entry)?;
            }
            Ok(None) => {}
            Err(error)
                if error
                    .downcast_ref::<Problem>()
                    .is_some_and(|problem| matches!(problem, Problem::Missing { .. })) =>
            {
                needed.push(entry)
            }
            Err(error) => return Err(error),
        }
    }
    let Some(first) = needed.first() else {
        return Ok(());
    };
    let source = Store::open(source).map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            Box::new(Problem::Missing {
                path: first.path.clone(),
            }) as Box<dyn std::error::Error + Send + Sync>
        } else {
            error
        }
    })?;
    for entry in needed {
        target.import(&source, entry)?;
    }
    Ok(())
}

/// A checkpoint's temporary clean filter writes new payloads into `captured`.
/// Unchanged pointers may refer to payloads already in the durable store. Check
/// both sets and sync every referenced payload before publishing a Git ref.
pub fn publish(captured: &Path, durable: &Path, entries: &[Entry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let captured = Store::open(captured)?;
    let durable = Store::create(durable)?;
    for entry in entries {
        match captured.verified(entry) {
            Ok(_) => durable.import(&captured, entry)?,
            Err(error)
                if error
                    .downcast_ref::<Problem>()
                    .is_some_and(|problem| matches!(problem, Problem::Missing { .. })) =>
            {
                durable.import(&durable, entry)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn sync(path: &Path, entries: &[Entry]) -> Result<()> {
    for entry in entries {
        if let Some(file) = read(path, entry)? {
            full_sync(&file)?;
            Store::open(path)?.sync_directories(entry)?;
        }
    }
    Ok(())
}

pub fn read(path: &Path, entry: &Entry) -> Result<Option<File>> {
    if entry.pointer.size() == 0 {
        return if entry.pointer.oid() == EMPTY {
            Ok(None)
        } else {
            Err(Problem::Corrupt {
                path: entry.path.clone(),
            }
            .into())
        };
    }
    let store = Store::open(path).map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            Box::new(Problem::Missing {
                path: entry.path.clone(),
            }) as Box<dyn std::error::Error + Send + Sync>
        } else {
            error
        }
    })?;
    store.verified(entry)
}

#[derive(Debug)]
pub enum Problem {
    Missing { path: String },
    Corrupt { path: String },
}
impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => {
                write!(formatter, "LFS object missing · {path} · can’t commit")
            }
            Self::Corrupt { path } => write!(
                formatter,
                "LFS object does not match its pointer · {path} · can’t commit"
            ),
        }
    }
}
impl std::error::Error for Problem {}

fn full_sync(file: &File) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn directory(parent: &File, name: &CStr, create: bool) -> Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 && create && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(std::io::Error::last_os_error().into());
        }
        full_sync(parent)?;
        fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn root(path: &Path, create: bool) -> Result<File> {
    if !path.is_absolute() {
        return Err("LFS storage needs an absolute physical path.".into());
    }
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                parent = directory(&parent, &CString::new(name.as_bytes())?, create)?
            }
            _ => return Err("LFS storage needs a normalized physical path.".into()),
        }
    }
    Ok(parent)
}
fn open_file(parent: &File, name: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err("An LFS object must be a regular file.".into());
    }
    Ok(file)
}
fn checked(mut file: File, entry: &Entry) -> Result<File> {
    if file.metadata()?.len() != entry.pointer.size() {
        return Err(Problem::Corrupt {
            path: entry.path.clone(),
        }
        .into());
    }
    let mut digest = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    let mut reader = (&mut file).take(entry.pointer.size() + 1);
    loop {
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
        total += size as u64;
    }
    let actual = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if total != entry.pointer.size() || actual != entry.pointer.oid() {
        return Err(Problem::Corrupt {
            path: entry.path.clone(),
        }
        .into());
    }
    file.rewind()?;
    Ok(file)
}

pub struct Store {
    root: File,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            root: root(path, false)?,
        })
    }
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            root: root(path, true)?,
        })
    }
    fn parent(&self, entry: &Entry, create: bool) -> Result<File> {
        let objects = directory(&self.root, c"objects", create)?;
        let first = directory(&objects, &CString::new(&entry.pointer.oid()[..2])?, create)?;
        directory(&first, &CString::new(&entry.pointer.oid()[2..4])?, create)
    }
    fn sync_directories(&self, entry: &Entry) -> Result<()> {
        let objects = directory(&self.root, c"objects", false)?;
        let first = directory(&objects, &CString::new(&entry.pointer.oid()[..2])?, false)?;
        let second = directory(&first, &CString::new(&entry.pointer.oid()[2..4])?, false)?;
        for directory in [&second, &first, &objects, &self.root] {
            full_sync(directory)?;
        }
        Ok(())
    }
    /// Empty LFS files have no stored payload. Other files are verified before
    /// their descriptor is returned to a scanner or an object transfer.
    pub fn verified(&self, entry: &Entry) -> Result<Option<File>> {
        if entry.pointer.size() == 0 {
            return if entry.pointer.oid() == EMPTY {
                Ok(None)
            } else {
                Err(Problem::Corrupt {
                    path: entry.path.clone(),
                }
                .into())
            };
        }
        let result = (|| {
            let parent = self.parent(entry, false)?;
            checked(
                open_file(&parent, &CString::new(entry.pointer.oid())?)?,
                entry,
            )
            .map(Some)
        })();
        result.map_err(|error: Box<dyn std::error::Error + Send + Sync>| {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                Problem::Missing {
                    path: entry.path.clone(),
                }
                .into()
            } else {
                error
            }
        })
    }
    /// APFS cloning creates an independent payload. Validate the clone as well
    /// as the source so a concurrent source write cannot publish a wrong OID.
    pub fn import(&self, source: &Self, entry: &Entry) -> Result<()> {
        let Some(source) = source.verified(entry)? else {
            return Ok(());
        };
        let parent = self.parent(entry, true)?;
        let name = CString::new(entry.pointer.oid())?;
        match self.verified(entry) {
            Ok(Some(existing)) => {
                full_sync(&existing)?;
                self.sync_directories(entry)?;
                return Ok(());
            }
            Ok(None) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<Problem>()
                    .is_some_and(|problem| matches!(problem, Problem::Missing { .. })) => {}
            Err(error) => return Err(error),
        }
        let temporary = directory(&self.root, c"tmp", true)?;
        let staging = CString::new(format!("intercat-{}", Uuid::now_v7()))?;
        if unsafe {
            libc::fclonefileat(
                source.as_raw_fd(),
                temporary.as_raw_fd(),
                staging.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let _staging = Staging {
            directory: &temporary,
            name: &staging,
        };
        let clone = checked(open_file(&temporary, &staging)?, entry)?;
        full_sync(&clone)?;
        if unsafe {
            libc::renameatx_np(
                temporary.as_raw_fd(),
                staging.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_EXCL,
            )
        } != 0
        {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
                return Err(std::io::Error::last_os_error().into());
            }
            let existing = self
                .verified(entry)?
                .ok_or("The LFS payload disappeared.")?;
            full_sync(&existing)?;
        }
        full_sync(&parent)?;
        full_sync(&temporary)?;
        self.sync_directories(entry)
    }
}
struct Staging<'a> {
    directory: &'a File,
    name: &'a CStr,
}
impl Drop for Staging<'_> {
    fn drop(&mut self) {
        unsafe {
            libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use intercat_git::lfs::Pointer;
    use std::{fs, os::unix::fs::symlink, path::PathBuf};

    fn entry(path: &str, bytes: &[u8]) -> Entry {
        let oid = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            bytes.len()
        );
        Entry {
            path: path.into(),
            pointer: Pointer::parse(pointer.as_bytes()).unwrap(),
        }
    }
    fn payload(root: &Path, entry: &Entry) -> PathBuf {
        root.join("objects")
            .join(&entry.pointer.oid()[..2])
            .join(&entry.pointer.oid()[2..4])
            .join(entry.pointer.oid())
    }
    fn write(root: &Path, entry: &Entry, bytes: &[u8]) -> Result<PathBuf> {
        let path = payload(root, entry);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, bytes)?;
        Ok(path)
    }
    fn private() -> Result<(tempfile::TempDir, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let physical = temporary.path().canonicalize()?;
        Ok((temporary, physical))
    }

    #[test]
    fn imports_only_requested_payload_and_keeps_cow_independent() -> Result<()> {
        let (_temporary, root) = private()?;
        let source = root.join("source");
        let destination = root.join("destination");
        let wanted = entry("assets/example.txt", b"dummy current payload\n");
        let historical = entry("assets/history.txt", b"dummy historical payload\n");
        let original = write(&source, &wanted, b"dummy current payload\n")?;
        write(&source, &historical, b"dummy historical payload\n")?;
        let store = Store::create(&destination)?;
        store.import(&Store::open(&source)?, &wanted)?;
        assert!(!payload(&destination, &historical).exists());
        fs::write(original, b"changed source bytes\n")?;
        let mut bytes = Vec::new();
        store.verified(&wanted)?.unwrap().read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"dummy current payload\n");
        assert_eq!(fs::read_dir(destination.join("tmp"))?.count(), 0);
        Ok(())
    }

    #[test]
    fn verified_private_payload_needs_no_public_copy_and_corruption_still_stops() -> Result<()> {
        let (_temporary, root) = private()?;
        let source = root.join("unpublished-source");
        let destination = root.join("private-target");
        let wanted = entry("dummy.txt", b"a private repaired payload");
        let file = write(&destination, &wanted, b"a private repaired payload")?;
        transfer(&source, &destination, std::slice::from_ref(&wanted))?;
        assert!(!source.exists());
        fs::write(file, b"corrupt private payload")?;
        let error = transfer(&source, &destination, &[wanted]).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Problem>(),
            Some(Problem::Corrupt { .. })
        ));
        assert!(!source.exists());
        Ok(())
    }

    #[test]
    fn repeated_import_reuses_verified_object_and_never_overwrites_corruption() -> Result<()> {
        let (_temporary, root) = private()?;
        let source = root.join("source");
        let destination = root.join("destination");
        let wanted = entry("dummy.txt", b"a dummy payload");
        write(&source, &wanted, b"a dummy payload")?;
        let store = Store::create(&destination)?;
        let source = Store::open(&source)?;
        store.import(&source, &wanted)?;
        store.import(&source, &wanted)?;
        let target = payload(&destination, &wanted);
        fs::write(&target, b"a wrong payload")?;
        assert!(matches!(
            store.import(&source, &wanted).unwrap_err().downcast_ref(),
            Some(Problem::Corrupt { path }) if path == "dummy.txt"
        ));
        assert_eq!(fs::read(target)?, b"a wrong payload");
        Ok(())
    }

    #[test]
    fn missing_wrong_size_and_wrong_hash_are_not_published() -> Result<()> {
        let (_temporary, root) = private()?;
        let source = root.join("source");
        let destination = root.join("destination");
        let wanted = entry("dummy.txt", b"expected");
        let source_store = Store::create(&source)?;
        let store = Store::create(&destination)?;
        let error = store.import(&source_store, &wanted).unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(Problem::Missing { path }) if path == "dummy.txt")
        );
        for bytes in [b"short".as_slice(), b"bad hash".as_slice()] {
            write(&source, &wanted, bytes)?;
            let error = store.import(&source_store, &wanted).unwrap_err();
            assert!(matches!(
                error.downcast_ref(),
                Some(Problem::Corrupt { .. })
            ));
            assert!(!payload(&destination, &wanted).exists());
        }
        Ok(())
    }

    #[test]
    fn refuses_symlinked_root_shards_and_payloads() -> Result<()> {
        let (_temporary, root) = private()?;
        let actual = root.join("actual");
        fs::create_dir(&actual)?;
        symlink(&actual, root.join("alias"))?;
        assert!(Store::open(&root.join("alias")).is_err());
        assert!(Store::create(&root.join("alias/new")).is_err());
        assert!(!actual.join("new").exists());

        let wanted = entry("dummy.txt", b"dummy payload");
        let outside = write(&actual, &wanted, b"dummy payload")?;
        let source = root.join("source");
        let store = Store::create(&source)?;
        symlink(actual.join("objects"), source.join("objects"))?;
        assert!(store.verified(&wanted).is_err());
        fs::remove_file(source.join("objects"))?;
        let path = payload(&source, &wanted);
        fs::create_dir_all(path.parent().unwrap())?;
        symlink(&outside, &path)?;
        assert!(store.verified(&wanted).is_err());
        let destination = Store::create(&root.join("destination"))?;
        assert!(destination.import(&store, &wanted).is_err());
        assert_eq!(fs::read(outside)?, b"dummy payload");
        Ok(())
    }

    #[test]
    fn empty_pointer_needs_no_payload_but_still_requires_empty_hash() -> Result<()> {
        let (_temporary, root) = private()?;
        let store = Store::create(&root.join("empty"))?;
        let empty = entry("empty.txt", b"");
        assert!(store.verified(&empty)?.is_none());
        store.import(&store, &empty)?;
        let invalid = Entry {
            path: "invalid.txt".into(),
            pointer: Pointer::parse(
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 0\n",
                    "0".repeat(64)
                )
                .as_bytes(),
            )
            .unwrap(),
        };
        assert!(matches!(
            store.verified(&invalid).unwrap_err().downcast_ref(),
            Some(Problem::Corrupt { .. })
        ));
        assert!(!root.join("empty/objects").exists());
        Ok(())
    }
}
