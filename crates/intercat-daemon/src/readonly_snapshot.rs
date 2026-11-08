//! A private export of a saved tree. It has no Git metadata, filters, hooks or
//! connection to a mutable checkout. Links are resolved only within that tree.
use crate::Result;
use intercat_git::{ObjectId, Repository, SavedFile};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

const LIMIT: u64 = 512 * 1024 * 1024;
pub(crate) struct Snapshot {
    directory: Option<tempfile::TempDir>,
    pub roots: Vec<PathBuf>,
    pub omitted: Vec<String>,
    bytes: u64,
}
impl Snapshot {
    pub fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        let directory = tempfile::Builder::new()
            .prefix("snapshot-")
            .tempdir_in(&parent)?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            directory: Some(directory),
            roots: vec![],
            omitted: vec![],
            bytes: 0,
        })
    }
    pub fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
    pub fn retain(mut self) -> PathBuf {
        self.directory.take().unwrap().keep()
    }
    pub fn export(&mut self, source: &Path, sha: ObjectId, name: &str) -> Result<Vec<String>> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err("Invalid snapshot name.".into());
        }
        let root = self.path().join(name);
        fs::create_dir(&root)?;
        let repo = Repository::open(source)?;
        let files = repo.saved_files(sha)?;
        let mut copied = Vec::new();
        for (path, file) in &files {
            if !safe(path) {
                self.omitted.push(format!("{name}/{path}"));
                continue;
            }
            let Some(file) = resolve(&repo, &files, path, file)? else {
                self.omitted.push(format!("{name}/{path}"));
                continue;
            };
            let size = repo.saved_size(file)?;
            self.bytes = self
                .bytes
                .checked_add(size)
                .ok_or("Snapshot size overflow.")?;
            if self.bytes > LIMIT {
                return Err("Saved trees exceed the 512 MiB planning limit.".into());
            }
            let destination = root.join(path);
            fs::create_dir_all(destination.parent().unwrap())?;
            // The host filesystem may fold case or Unicode names even when
            // Git does not. Never overwrite an earlier saved entry on export.
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            output.write_all(&repo.saved_bytes(file)?)?;
            fs::set_permissions(destination, fs::Permissions::from_mode(0o400))?;
            copied.push(path.clone());
        }
        self.roots.push(root);
        Ok(copied)
    }
    pub fn freeze(&self) -> Result<()> {
        for entry in walkdir::WalkDir::new(self.path()).contents_first(true) {
            let entry = entry?;
            if entry.file_type().is_dir() {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o500))?;
            }
        }
        Ok(())
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if self.directory.is_none() {
            return;
        }
        // Only this object's private export is made removable again.
        for entry in walkdir::WalkDir::new(self.path())
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if entry.file_type().is_dir() {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700));
            }
        }
    }
}
fn safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && Path::new(path).components().all(|part| match part {
            Component::Normal(name) => {
                !name.eq_ignore_ascii_case(".git")
                    && name
                        .to_str()
                        .is_some_and(|name| !intercat_git::secret_path(name))
            }
            _ => false,
        })
}
fn resolve<'a>(
    repo: &Repository,
    files: &'a BTreeMap<String, SavedFile>,
    path: &str,
    file: &'a SavedFile,
) -> Result<Option<&'a SavedFile>> {
    let mut path = path.to_owned();
    let mut file = file;
    let mut seen = BTreeSet::new();
    for _ in 0..40 {
        if !seen.insert(path.clone()) {
            return Ok(None);
        }
        match file.mode {
            0o100644 | 0o100755 => return Ok(Some(file)),
            0o120000 => {
                let bytes = repo.saved_bytes(file)?;
                let target = match std::str::from_utf8(&bytes) {
                    Ok(target) => target,
                    Err(_) => return Ok(None),
                };
                let mut resolved = Path::new(&path).parent().unwrap().to_path_buf();
                for part in Path::new(target).components() {
                    match part {
                        Component::Normal(name) => resolved.push(name),
                        Component::CurDir => {}
                        Component::ParentDir if resolved.pop() => {}
                        _ => return Ok(None),
                    }
                }
                path = match resolved.to_str() {
                    Some(path) if safe(path) => path.to_owned(),
                    _ => return Ok(None),
                };
                let Some(next) = files.get(&path) else {
                    return Ok(None);
                };
                file = next;
            }
            _ => return Err("Unsupported saved file mode.".into()),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exports_saved_bytes_and_internal_links_without_secrets_or_checkout_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("project");
        fs::create_dir(&source)?;
        Repository::init_isolated(&source)?;
        let repo = Repository::open(&source)?;
        let text = repo.write_blob(b"saved dummy file")?;
        let secret = repo.write_blob(b"dummy secret marker")?;
        let internal = repo.write_blob(b"../readme.txt")?;
        let outside = repo.write_blob(b"/etc/hosts")?;
        let secret_link = repo.write_blob(b".env")?;
        let tree = repo.write_tree(&[
            ("readme.txt", text),
            (".env", secret),
            ("nested/link.txt", internal),
            ("outside.txt", outside),
            ("secret-link.txt", secret_link),
        ])?;
        // write_tree creates regular blobs. Replace the intended links with
        // their Git modes through a private index, never the source checkout.
        let index = root.path().join("index");
        let run = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("/usr/bin/git")
                .current_dir(&source)
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).to_string().into());
            }
            Ok(())
        };
        run(&["read-tree", &tree.to_string()])?;
        for (name, blob) in [
            ("nested/link.txt", internal),
            ("outside.txt", outside),
            ("secret-link.txt", secret_link),
        ] {
            run(&[
                "update-index",
                "--cacheinfo",
                &format!("120000,{blob},{name}"),
            ])?;
        }
        let tree = repo.tree_from_index(&index)?;
        let commit = repo.write_commit(
            tree,
            &[],
            "dummy snapshot",
            gix::actor::Signature {
                name: "Dummy".into(),
                email: "dummy@localhost".into(),
                time: gix::date::Time::now_local_or_utc(),
            },
        )?;
        fs::write(source.join("readme.txt"), "unsaved checkout text")?;
        let path;
        {
            let mut snapshot = Snapshot::new(&root.path().join("snapshots"))?;
            snapshot.export(&source, commit, "session")?;
            assert_eq!(
                fs::read(snapshot.path().join("session/readme.txt"))?,
                b"saved dummy file"
            );
            assert_eq!(
                fs::read(snapshot.path().join("session/nested/link.txt"))?,
                b"saved dummy file"
            );
            assert!(!snapshot.path().join("session/.env").exists());
            assert!(!snapshot.path().join("session/outside.txt").exists());
            assert!(!snapshot.path().join("session/secret-link.txt").exists());
            assert!(!snapshot.path().join("session/.git").exists());
            snapshot.freeze()?;
            path = snapshot.path().to_path_buf();
        }
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(source.join("readme.txt"))?,
            "unsaved checkout text"
        );
        Ok(())
    }
}
