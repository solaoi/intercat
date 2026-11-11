//! Human choices are saved as a private object before reservation. Applying
//! them uses a disposable Check generation; the held merge files stay intact.
use crate::{
    Result,
    merge_content::{self, Version},
    merge_manual::{Apply, Edit, Side, Snapshot},
    merge_workspace::{Resolved, Workspace},
};
use intercat_git::{Repository, SavedFile};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
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
    path::{Component, Path, PathBuf},
};
use tokio::sync::watch;

const LIMIT: u64 = 1024 * 1024;
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub sides: Sides,
    pub version: String,
    pub text: Option<String>,
    pub editable: bool,
}
#[derive(Serialize)]
pub struct Sides {
    pub path: String,
    pub target: Option<Version>,
    pub incoming: Option<Version>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: uuid::Uuid,
    pub revision: u64,
    pub paths: Vec<String>,
    pub file: FileView,
}
struct Content {
    version: String,
    text: Option<String>,
}
fn content(root: &Path, path: &str) -> Result<Content> {
    let parts = Path::new(path).components().collect::<Vec<_>>();
    if parts.is_empty()
        || parts.iter().any(
            |part| !matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(".git")),
        )
    {
        return Err("Invalid conflict file path.".into());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            unreachable!()
        };
        let name = CString::new(name.as_bytes())?;
        let last = index + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if last { 0 } else { libc::O_DIRECTORY };
        // Every parent is resolved relative to an open directory descriptor.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(Content {
                    version: "absent".into(),
                    text: None,
                });
            }
            if last && error.raw_os_error() == Some(libc::ELOOP) {
                let mut bytes = vec![0u8; 4096];
                let count = unsafe {
                    libc::readlinkat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                };
                if count < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                bytes.truncate(count as usize);
                return Ok(Content {
                    version: format!("link:{}", digest(&bytes)),
                    text: None,
                });
            }
            return Err(
                "A conflict path has an unsafe parent or file type. No content was read.".into(),
            );
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if !last {
            directory = file;
            continue;
        }
        let metadata = file.metadata()?;
        if metadata.is_file() && metadata.nlink() != 1 {
            return Err("A conflict file has shared hard links. No content was read.".into());
        }
        if !metadata.is_file() || metadata.len() > LIMIT {
            return Ok(Content {
                version: format!(
                    "type:{}:{}:{}:{}:{}",
                    metadata.mode(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec()
                ),
                text: None,
            });
        }
        let mut bytes = Vec::new();
        (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err("The conflict file grew while being read.".into());
        }
        let version = format!("{}:{}", metadata.mode() & 0o777, digest(&bytes));
        return Ok(Content {
            version,
            text: merge_content::text(bytes),
        });
    }
    unreachable!()
}
fn path(edit: &Edit) -> (&str, &str) {
    match edit {
        Edit::Text { path, version, .. }
        | Edit::Side { path, version, .. }
        | Edit::Current { path, version } => (path, version),
    }
}
fn verify(workspace: &Workspace, request: &Apply) -> Result<()> {
    let expected = merge_content::paths(&workspace.original, &workspace.input)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let selected = request
        .files
        .iter()
        .map(|edit| path(edit).0.to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    if request.id != workspace.input.marker
        || request.files.len() != selected.len()
        || selected != expected
    {
        return Err("Choose a resolution for every conflict file exactly once.".into());
    }
    for edit in &request.files {
        let (path, version) = path(edit);
        let sides = merge_content::load(&workspace.original, &workspace.input, path)?;
        let current = content(&workspace.checkout.workdir, path)?;
        if current.version != version {
            return Err(
                "A conflict file changed. Read it again before applying your choices.".into(),
            );
        }
        match edit {
            Edit::Side {..} => {},
            Edit::Text { text, .. } if sides.editable() && text.len() as u64 <= LIMIT && !text.contains('\0') && !crate::merge_manual::has_markers(text.as_bytes(), workspace.input.marker) => {},
            Edit::Current {..} if sides.editable() && current.text.as_ref().is_some_and(|text| !crate::merge_manual::has_markers(text.as_bytes(), workspace.input.marker)) => {},
            _ => return Err("Text resolutions must contain no Intercat markers. Binary, link and deletion conflicts require an explicit side.".into()),
        }
    }
    Ok(())
}
pub fn view(
    workspace: &Workspace,
    run: &crate::repair_run::Run,
    path: Option<&str>,
) -> Result<View> {
    let paths = merge_content::paths(&workspace.original, &workspace.input)?;
    let selected = path
        .or_else(|| paths.first().map(String::as_str))
        .ok_or("No conflict files remain.")?;
    let sides = merge_content::load(&workspace.original, &workspace.input, selected)?;
    let current = content(&workspace.checkout.workdir, selected)?;
    let editable = sides.editable() && current.text.is_some();
    Ok(View {
        id: run.id,
        revision: run.revision,
        paths,
        file: FileView {
            sides: Sides {
                path: sides.path,
                target: sides.target,
                incoming: sides.incoming,
            },
            version: current.version,
            text: current.text,
            editable,
        },
    })
}
pub fn save(workspace: &Workspace, request: &Apply) -> Result<Snapshot> {
    verify(workspace, request)?;
    let bytes = serde_json::to_vec(request)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("The manual resolution exceeds 8 MiB.".into());
    }
    let repo = Repository::open(&workspace.original)?;
    let sha = repo.write_blob(&bytes)?;
    let reference = format!("refs/intercat/manual/{}/{}", request.id, request.op_id);
    repo.sync_objects(&[sha])?;
    repo.compare_and_swap(&reference, None, sha)?;
    repo.sync_ref(&reference)?;
    Ok(Snapshot {
        operation: request.op_id,
        reference,
        sha: sha.to_string(),
        digest: digest(&bytes),
    })
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub async fn resolve(
    workspace: &Workspace,
    snapshot: &Snapshot,
    cancel: watch::Receiver<bool>,
) -> Result<Resolved> {
    let original = Repository::open(&workspace.original)?;
    let sha = snapshot.sha.parse()?;
    if original.reference_target(&snapshot.reference)? != Some(sha) {
        return Err("The saved manual resolution changed.".into());
    }
    let bytes = original.saved_bytes(&SavedFile {
        object: sha,
        mode: 0o100644,
    })?;
    if digest(&bytes) != snapshot.digest {
        return Err("The saved manual request changed.".into());
    }
    let request: Apply = serde_json::from_slice(&bytes)?;
    if request.op_id != snapshot.operation {
        return Err("The saved resolution belongs to another operation.".into());
    }
    verify(workspace, &request)?;
    let mut checkout = workspace
        .checkout
        .next_check(workspace.input.head.parse()?, cancel.clone())
        .await?;
    let _scratch = Scratch(checkout.root.clone());
    checkout
        .configuration
        .denied_roots
        .push(workspace.checkout.root.clone());
    let copy = Workspace {
        checkout,
        input: workspace.input.clone(),
        original: workspace.original.clone(),
    };
    verify(&copy, &request)?;
    let mut replacements = Vec::new();
    for edit in &request.files {
        let (path, _) = path(edit);
        let sides = merge_content::load(&workspace.original, &workspace.input, path)?;
        match edit {
            Edit::Text { text, .. } => crate::restoration::write_file(
                &copy.checkout.workdir,
                path,
                sides.target.as_ref().unwrap().mode,
                text.as_bytes(),
            )?,
            Edit::Side { side, .. } => {
                let version = match side {
                    Side::Target => sides.target,
                    Side::Incoming => sides.incoming,
                };
                replacements.push((
                    path.to_owned(),
                    version
                        .map(|version| -> Result<SavedFile> {
                            Ok(SavedFile {
                                object: version.object.parse()?,
                                mode: version.mode,
                            })
                        })
                        .transpose()?,
                ));
            }
            Edit::Current { .. } => {}
        }
    }
    copy.checkout
        .stage(workspace.input.head.parse()?, cancel.clone())
        .await?;
    let repo = Repository::open(&copy.checkout.workdir)?;
    let tree = repo.replace_files(
        repo.tree_from_index(&copy.checkout.workdir.join(".git/index"))?,
        &replacements,
    )?;
    copy.checkout.deploy(tree, cancel.clone()).await?;
    let plan = copy.finish(cancel).await?;
    if plan.tree.parse::<intercat_git::ObjectId>()? != tree {
        return Err(
            "File conversion changed the chosen resolution. Review the result again.".into(),
        );
    }
    verify(workspace, &request)?;
    let target = Repository::open(&workspace.checkout.workdir)?;
    target.import_tree(&copy.checkout.workdir, tree)?;
    crate::lfs_store::transfer(
        &copy.checkout.workdir.join(".git/lfs"),
        &workspace.checkout.workdir.join(".git/lfs"),
        &target.lfs_entries(tree)?,
    )?;
    let reference = format!("refs/intercat/manual-results/{}", snapshot.operation);
    target.sync_objects(&[tree])?;
    target.compare_and_swap(&reference, None, tree)?;
    target.sync_ref(&reference)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn working_file_reads_do_not_follow_links_or_accept_stale_content() -> Result<()> {
        let dummy = tempfile::tempdir()?;
        let root = dummy.path().join("merge");
        std::fs::create_dir(&root)?;
        let outside = dummy.path().join("dummy-reference.txt");
        std::fs::write(
            &outside,
            "agent-created reference, never display through a link",
        )?;
        std::fs::write(root.join("file.txt"), "first\r\n")?;
        let first = content(&root, "file.txt")?;
        std::fs::write(root.join("file.txt"), "second\r\n")?;
        assert_ne!(first.version, content(&root, "file.txt")?.version);
        std::fs::remove_file(root.join("file.txt"))?;
        std::os::unix::fs::symlink(&outside, root.join("file.txt"))?;
        let linked = content(&root, "file.txt")?;
        assert!(linked.text.is_none());
        assert!(linked.version.starts_with("link:"));
        std::fs::remove_file(root.join("file.txt"))?;
        std::fs::hard_link(&outside, root.join("file.txt"))?;
        assert!(content(&root, "file.txt").is_err());
        std::os::unix::fs::symlink(dummy.path(), root.join("parent"))?;
        assert!(content(&root, "parent/dummy-reference.txt").is_err());
        for path in [
            "../dummy-reference.txt",
            ".git/config",
            "/file.txt",
            "parent/../file.txt",
        ] {
            assert!(content(&root, path).is_err());
        }
        assert_eq!(content(&root, "missing/file.txt")?.version, "absent");
        Ok(())
    }
}
