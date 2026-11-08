//! Apply an object-only tree delta without touching the worker's index or HEAD.
//! Each path must still match the saved before or after value on every retry.
use crate::Result;
use intercat_git::{ObjectId, Repository};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path},
};

#[derive(Clone, PartialEq, Eq)]
struct File {
    mode: u16,
    bytes: Vec<u8>,
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

fn load(repo: &Repository, tree: ObjectId) -> Result<BTreeMap<String, File>> {
    repo.tree_files(tree)?
        .into_iter()
        .map(|(path, file)| {
            if Path::new(&path)
                .components()
                .any(|part| !matches!(part,Component::Normal(name) if name!=".git"))
            {
                return Err("Invalid inherited path.".into());
            }
            if ![0o100644, 0o100755, 0o120000].contains(&file.mode) {
                return Err("Unsupported inherited file type.".into());
            }
            Ok((
                path,
                File {
                    mode: file.mode,
                    bytes: repo.saved_bytes(&file)?,
                },
            ))
        })
        .collect()
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
        Ok(meta) if meta.file_type().is_symlink() => Ok(Observed::File(File {
            mode: 0o120000,
            bytes: fs::read_link(target)?.as_os_str().as_bytes().to_vec(),
        })),
        Ok(meta) if meta.is_dir() => Ok(Observed::Directory),
        Ok(meta) if meta.is_file() => {
            let mut bytes = Vec::new();
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(target)?
                .read_to_end(&mut bytes)?;
            Ok(Observed::File(File {
                mode: if meta.permissions().mode() & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                },
                bytes,
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

pub(crate) fn apply(root: &Path, before: ObjectId, after: ObjectId) -> Result<usize> {
    crate::deletion::validate_parent(root, "")?;
    let repo = Repository::open(root)?;
    let before = load(&repo, before)?;
    let after = load(&repo, after)?;
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
        crate::restoration::write_file(root, path, file.mode, &file.bytes)?;
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
    fn tree(repo: &Repository, files: &[(&str, &[u8])]) -> Result<ObjectId> {
        let entries = files
            .iter()
            .map(|(path, bytes)| Ok((*path, repo.write_blob(bytes)?)))
            .collect::<Result<Vec<_>>>()?;
        repo.write_tree(&entries)
    }
    #[test]
    fn replay_keeps_unrelated_files_and_accepts_a_partly_applied_delta() -> Result<()> {
        let (root, repo) = fixture()?;
        let path = root.path();
        let before = tree(&repo, &[("a", b"old"), ("z", b"remove")])?;
        let after = tree(&repo, &[("a", b"new"), ("binary", &[0, 1, 255])])?;
        fs::write(path.join("a"), b"new")?;
        fs::write(path.join("z"), b"remove")?;
        fs::write(path.join("outside"), b"untouched")?;
        assert_eq!(apply(path, before, after)?, 3);
        assert_eq!(fs::read(path.join("binary"))?, vec![0, 1, 255]);
        assert!(!path.join("z").exists());
        assert_eq!(fs::read(path.join("outside"))?, b"untouched");
        assert_eq!(apply(path, before, after)?, 3);
        assert!(!path.join(".git/index").exists());
        Ok(())
    }
    #[test]
    fn a_separate_edit_rejects_the_whole_delta_before_its_first_write() -> Result<()> {
        let (root, repo) = fixture()?;
        let path = root.path();
        let before = tree(&repo, &[("a", b"old"), ("z", b"old")])?;
        let after = tree(&repo, &[("a", b"new"), ("z", b"new")])?;
        fs::write(path.join("a"), b"old")?;
        fs::write(path.join("z"), b"user edit")?;
        assert!(apply(path, before, after).is_err());
        assert_eq!(fs::read(path.join("a"))?, b"old");
        assert_eq!(fs::read(path.join("z"))?, b"user edit");
        Ok(())
    }
    #[test]
    fn files_and_directories_can_replace_each_other_but_never_delete_foreign_children() -> Result<()>
    {
        let (root, repo) = fixture()?;
        let path = root.path();
        let flat = tree(&repo, &[("entry", b"flat")])?;
        let nested = tree(&repo, &[("entry/sub/file", b"nested")])?;
        fs::write(path.join("entry"), b"flat")?;
        apply(path, flat, nested)?;
        assert_eq!(fs::read(path.join("entry/sub/file"))?, b"nested");
        fs::write(path.join("entry/foreign"), b"keep")?;
        assert!(apply(path, nested, flat).is_err());
        assert_eq!(fs::read(path.join("entry/sub/file"))?, b"nested");
        fs::remove_file(path.join("entry/foreign"))?;
        apply(path, nested, flat)?;
        assert_eq!(fs::read(path.join("entry"))?, b"flat");
        Ok(())
    }
    #[test]
    fn an_unexpected_parent_symlink_cannot_redirect_an_inherited_write() -> Result<()> {
        let (root, repo) = fixture()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("sentinel"), b"keep")?;
        std::os::unix::fs::symlink(outside.path(), root.path().join("folder"))?;
        let before = tree(&repo, &[])?;
        let after = tree(&repo, &[("folder/sentinel", b"replace")])?;
        assert!(apply(root.path(), before, after).is_err());
        assert_eq!(fs::read(outside.path().join("sentinel"))?, b"keep");
        Ok(())
    }
}
