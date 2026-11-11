//! Apply an isolated, converted file delta without touching the worker's index or HEAD.
//! Each path must still match the saved before or after value on every retry.
use crate::Result;
use intercat_git::{ObjectId, Repository};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

#[derive(Clone)]
enum Body {
    Regular(PathBuf),
    Link(Vec<u8>),
}
#[derive(Clone)]
struct File {
    mode: u16,
    size: u64,
    digest: [u8; 32],
    body: Body,
}
impl PartialEq for File {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.size == other.size && self.digest == other.digest
    }
}
impl Eq for File {}

/// Working bytes from a completed, sandboxed checkout, rather than raw Git
/// blobs. This also records CRLF, smudge filters and core.symlinks=false.
pub(crate) struct Image(BTreeMap<String, File>);
impl Image {
    pub(crate) fn capture(repo: &Repository, tree: ObjectId, root: &Path) -> Result<Self> {
        let mut files = BTreeMap::new();
        for (path, saved) in repo.tree_files(tree)? {
            if ![0o100644, 0o100755, 0o120000].contains(&saved.mode) {
                return Err("Unsupported inherited file type.".into());
            }
            crate::deletion::validate_parent(root, &path)?;
            let Observed::File(file) = observe(root, &path, &BTreeMap::new())? else {
                return Err(format!("{path}: the converted file is missing.").into());
            };
            if file.mode == 0o120000 && saved.mode != 0o120000 {
                return Err(
                    format!("{path}: a filter replaced a regular file with a link.").into(),
                );
            }
            files.insert(path, file);
        }
        Ok(Self(files))
    }
}
struct Change {
    before: Option<File>,
    after: Option<File>,
}
enum Observed {
    Absent,
    File(File),
    Directory,
}

fn regular(path: &Path) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err("A converted file is not a separate regular file.".into());
    }
    Ok(file)
}
fn digest(mut input: impl Read, mut output: impl Write) -> Result<(u64, [u8; 32])> {
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 8192];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((size, hash.finalize().into()))
}
impl File {
    fn write(&self, root: &Path, path: &str) -> Result<()> {
        match &self.body {
            Body::Link(bytes) => crate::restoration::write_file(root, path, self.mode, bytes),
            Body::Regular(source) => {
                crate::restoration::write_regular(root, path, self.mode, |out| {
                    let file = regular(source)?;
                    if (if file.metadata()?.permissions().mode() & 0o111 == 0 {
                        0o100644
                    } else {
                        0o100755
                    }) != self.mode
                    {
                        return Err("The converted source mode changed before application.".into());
                    }
                    let actual = digest(file, out)?;
                    if actual != (self.size, self.digest) {
                        return Err("The converted source changed before application.".into());
                    }
                    Ok(())
                })
            }
        }
    }
}
fn observe(root: &Path, path: &str, changes: &BTreeMap<String, Change>) -> Result<Observed> {
    let mut parent = String::new();
    let parts = Path::new(path).components().collect::<Vec<_>>();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        let Component::Normal(name) = part else {
            return Err("Invalid inherited path.".into());
        };
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push_str(name.to_str().ok_or("Non-UTF8 inherited path.")?);
        match fs::symlink_metadata(root.join(&parent)) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Observed::Absent);
            }
            Ok(_) if changes.contains_key(&parent) => return Ok(Observed::Absent),
            _ => return Err(format!("{path}: its parent is no longer a directory.").into()),
        }
    }
    let target = root.join(path);
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Observed::Absent),
        Err(error) => Err(error.into()),
        Ok(meta) if meta.file_type().is_symlink() => {
            let bytes = fs::read_link(target)?.as_os_str().as_bytes().to_vec();
            Ok(Observed::File(File {
                mode: 0o120000,
                size: bytes.len() as u64,
                digest: Sha256::digest(&bytes).into(),
                body: Body::Link(bytes),
            }))
        }
        Ok(meta) if meta.is_dir() => Ok(Observed::Directory),
        Ok(meta) if meta.is_file() => {
            let input = regular(&target)?;
            let meta = input.metadata()?;
            let (size, digest) = digest(input, std::io::sink())?;
            Ok(Observed::File(File {
                mode: if meta.permissions().mode() & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                },
                size,
                digest,
                body: Body::Regular(target),
            }))
        }
        _ => Err(format!("{path}: unsupported working file type.").into()),
    }
}
fn matches(value: &Observed, file: &Option<File>) -> bool {
    match (value, file) {
        (Observed::Absent, None) => true,
        (Observed::File(value), Some(file)) => value == file,
        _ => false,
    }
}
fn validate(
    root: &Path,
    path: &str,
    change: &Change,
    changes: &BTreeMap<String, Change>,
) -> Result<()> {
    let current = observe(root, path, changes)?;
    if matches(&current, &change.before) || matches(&current, &change.after) {
        return Ok(());
    }
    if matches!(current, Observed::Directory) {
        let prefix = format!("{path}/");
        if change.after.is_none() && changes.keys().any(|name| name.starts_with(&prefix)) {
            return Ok(());
        }
        if change.before.is_none() {
            for entry in walkdir::WalkDir::new(root.join(path)).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_dir() {
                    continue;
                }
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .ok_or("Non-UTF8 working file.")?;
                if !changes
                    .get(name)
                    .is_some_and(|change| change.after.is_none())
                {
                    return Err(format!("{name}: a separate edit prevents inherited files from replacing this directory.").into());
                }
            }
            return Ok(());
        }
    }
    Err(format!("{path}: changed since inheritance was prepared; your edit was kept.").into())
}
fn remove_empty_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err("A new working file prevents the inherited directory replacement.".into());
        }
        remove_empty_tree(&entry.path())?;
    }
    fs::remove_dir(path)?;
    Ok(())
}

pub(crate) fn apply(root: &Path, before: &Image, after: &Image) -> Result<usize> {
    crate::deletion::validate_parent(root, "")?;
    let before = &before.0;
    let after = &after.0;
    let mut changes = BTreeMap::new();
    for path in before.keys().chain(after.keys()) {
        if before.get(path) != after.get(path) {
            changes.insert(
                path.clone(),
                Change {
                    before: before.get(path).cloned(),
                    after: after.get(path).cloned(),
                },
            );
        }
    }
    for (path, change) in &changes {
        validate(root, path, change, &changes)?;
    }
    let mut deleted = changes
        .iter()
        .filter(|(_, change)| change.after.is_none())
        .collect::<Vec<_>>();
    deleted.sort_by_key(|(path, _)| std::cmp::Reverse(path.matches('/').count()));
    for (path, change) in deleted {
        validate(root, path, change, &changes)?;
        if matches!(observe(root, path, &changes)?, Observed::File(_)) {
            crate::deletion::validate_parent(root, path)?;
            fs::remove_file(root.join(path))?;
            crate::restoration::sync_directory(
                root.join(path).parent().ok_or("Invalid inherited path.")?,
            )?;
        }
    }
    for (path, change) in &changes {
        let Some(file) = &change.after else {
            continue;
        };
        validate(root, path, change, &changes)?;
        if matches(&observe(root, path, &changes)?, &change.after) {
            continue;
        }
        if fs::symlink_metadata(root.join(path)).is_ok_and(|meta| meta.is_dir()) {
            remove_empty_tree(&root.join(path))?;
        }
        file.write(root, path)?;
    }
    for (path, change) in &changes {
        let observed = observe(root, path, &changes)?;
        if matches(&observed, &change.after) {
            continue;
        }
        if change.after.is_none()
            && matches!(observed, Observed::Directory)
            && after
                .keys()
                .any(|name| name.starts_with(&format!("{path}/")))
        {
            continue;
        }
        return Err(
            format!("{path}: changed during inheritance; review the working files.").into(),
        );
    }
    Ok(changes.len())
}

pub(crate) fn verify(
    repo: &Repository,
    before: ObjectId,
    after: ObjectId,
    actual: ObjectId,
) -> Result<()> {
    let before = repo.tree_files(before)?;
    let after = repo.tree_files(after)?;
    let actual = repo.tree_files(actual)?;
    for path in before.keys().chain(after.keys()) {
        if before.get(path) != after.get(path) && actual.get(path) != after.get(path) {
            return Err(format!(
                "{path}: changed after inherited files were applied; your edit was kept."
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Result<(tempfile::TempDir, Repository)> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        Ok((root, repo))
    }
    fn image(repo: &Repository, files: &[(&str, &[u8])]) -> Result<Image> {
        let entries = files
            .iter()
            .map(|(path, bytes)| Ok((*path, repo.write_blob(bytes)?)))
            .collect::<Result<Vec<_>>>()?;
        let tree = repo.write_tree(&entries)?;
        let root = repo
            .common_directory()?
            .join(format!("converted-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        for (path, bytes) in files {
            crate::restoration::write_file(&root, path, 0o100644, bytes)?;
        }
        Image::capture(repo, tree, &root)
    }
    #[test]
    fn replay_keeps_unrelated_files_and_accepts_a_partly_applied_delta() -> Result<()> {
        let (root, repo) = fixture()?;
        let path = root.path();
        let before = image(&repo, &[("a", b"old"), ("z", b"remove")])?;
        let after = image(&repo, &[("a", b"new"), ("binary", &[0, 1, 255])])?;
        fs::write(path.join("a"), b"new")?;
        fs::write(path.join("z"), b"remove")?;
        fs::write(path.join("outside"), b"untouched")?;
        assert_eq!(apply(path, &before, &after)?, 3);
        assert_eq!(fs::read(path.join("binary"))?, vec![0, 1, 255]);
        assert!(!path.join("z").exists());
        assert_eq!(fs::read(path.join("outside"))?, b"untouched");
        assert_eq!(apply(path, &before, &after)?, 3);
        assert!(!path.join(".git/index").exists());
        Ok(())
    }
    #[test]
    fn a_separate_edit_rejects_the_whole_delta_before_its_first_write() -> Result<()> {
        let (root, repo) = fixture()?;
        let path = root.path();
        let before = image(&repo, &[("a", b"old"), ("z", b"old")])?;
        let after = image(&repo, &[("a", b"new"), ("z", b"new")])?;
        fs::write(path.join("a"), b"old")?;
        fs::write(path.join("z"), b"user edit")?;
        assert!(apply(path, &before, &after).is_err());
        assert_eq!(fs::read(path.join("a"))?, b"old");
        assert_eq!(fs::read(path.join("z"))?, b"user edit");
        Ok(())
    }
    #[test]
    fn files_and_directories_can_replace_each_other_but_never_delete_foreign_children() -> Result<()>
    {
        let (root, repo) = fixture()?;
        let path = root.path();
        let flat = image(&repo, &[("entry", b"flat")])?;
        let nested = image(&repo, &[("entry/sub/file", b"nested")])?;
        fs::write(path.join("entry"), b"flat")?;
        apply(path, &flat, &nested)?;
        assert_eq!(fs::read(path.join("entry/sub/file"))?, b"nested");
        fs::write(path.join("entry/foreign"), b"keep")?;
        assert!(apply(path, &nested, &flat).is_err());
        assert_eq!(fs::read(path.join("entry/sub/file"))?, b"nested");
        fs::remove_file(path.join("entry/foreign"))?;
        apply(path, &nested, &flat)?;
        assert_eq!(fs::read(path.join("entry"))?, b"flat");
        Ok(())
    }
    #[test]
    fn an_unexpected_parent_symlink_cannot_redirect_an_inherited_write() -> Result<()> {
        let (root, repo) = fixture()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("sentinel"), b"keep")?;
        std::os::unix::fs::symlink(outside.path(), root.path().join("folder"))?;
        let before = image(&repo, &[])?;
        let after = image(&repo, &[("folder/sentinel", b"replace")])?;
        assert!(apply(root.path(), &before, &after).is_err());
        assert_eq!(fs::read(outside.path().join("sentinel"))?, b"keep");
        Ok(())
    }
    #[test]
    fn a_changed_converted_source_is_never_renamed_over_the_working_file() -> Result<()> {
        let (root, repo) = fixture()?;
        let before = image(&repo, &[("file", b"before")])?;
        let after = image(&repo, &[("file", b"after")])?;
        fs::write(root.path().join("file"), b"before")?;
        let Body::Regular(source) = &after.0["file"].body else {
            panic!("regular fixture")
        };
        fs::write(source, b"changed after conversion")?;
        assert!(apply(root.path(), &before, &after).is_err());
        assert_eq!(fs::read(root.path().join("file"))?, b"before");
        assert!(!fs::read_dir(root.path())?.any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".intercat-restore-")
        }));
        Ok(())
    }
}
