//! A branch's chosen instruction files are immutable local objects. The journal
//! stores their hash and paths, never their text. No project filter runs here.
use crate::{
    Result,
    branch::{Branch, InstructionMode, InstructionSnapshot},
    instructions::{Files, is_instruction},
};
use intercat_git::{ObjectId, Repository, SavedFile};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Component, Path},
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct Frozen {
    before: Files,
    after: Files,
}

fn load(repo: &Repository, snapshot: &InstructionSnapshot) -> Result<Frozen> {
    let sha = ObjectId::from_hex(snapshot.sha.as_bytes())?;
    if repo.reference_target(&snapshot.reference)? != Some(sha) {
        return Err("The saved branch instructions are unavailable.".into());
    }
    let bytes = repo.saved_bytes(&SavedFile {
        object: sha,
        mode: 0o100644,
    })?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("The instruction snapshot exceeds the size limit.".into());
    }
    let frozen: Frozen = serde_json::from_slice(&bytes)?;
    for files in [&frozen.before, &frozen.after] {
        files.instructions()?;
    }
    Ok(frozen)
}

fn changed(frozen: &Frozen) -> Vec<String> {
    frozen
        .before
        .0
        .keys()
        .chain(frozen.after.0.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| frozen.before.0.get(*path) != frozen.after.0.get(*path))
        .cloned()
        .collect()
}

pub async fn freeze(branch: &Branch, source: &Path) -> Result<InstructionSnapshot> {
    let reference = format!("{}-instructions", branch.workspace.reference);
    {
        let repo = Repository::open(source)?;
        if let Some(sha) = repo.reference_target(&reference)? {
            let mut snapshot = InstructionSnapshot {
                reference,
                sha: sha.to_string(),
                changed: vec![],
            };
            snapshot.changed = changed(&load(&repo, &snapshot)?);
            repo.sync_objects(&[sha])?;
            repo.sync_ref(&snapshot.reference)?;
            return Ok(snapshot);
        }
    }
    let child = crate::source_snapshot::clean_project(&branch.workspace.path).await?;
    let repo = Repository::open(source)?;
    Repository::verify_prepared_worktree(&branch.workspace.path, &child)?;
    if child.head != ObjectId::from_hex(branch.point.sha().as_bytes())?
        || child.common_dir != repo.common_directory()?
    {
        return Err("The branch instruction source changed before it was frozen.".into());
    }
    let before = Files::capture(&branch.workspace.path)?;
    let after = match branch.request.instructions {
        InstructionMode::Current => Files::capture(source)?,
        InstructionMode::Checkpoint => before.clone(),
    };
    let frozen = Frozen { before, after };
    let encoded = serde_json::to_vec(&frozen)?;
    if encoded.len() > 8 * 1024 * 1024 {
        return Err("The instruction snapshot exceeds the size limit.".into());
    }
    let sha = repo.write_blob(&encoded)?;
    repo.sync_objects(&[sha])?;
    repo.compare_and_swap(&reference, None, sha)?;
    repo.sync_ref(&reference)?;
    Ok(InstructionSnapshot {
        reference,
        sha: sha.to_string(),
        changed: changed(&frozen),
    })
}

fn open_root(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}
fn sync(file: &File) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn parent(root: &File, path: &str, create: bool) -> Result<Option<(File, CString)>> {
    if !is_instruction(path) {
        return Err("Invalid instruction file path.".into());
    }
    let parts = Path::new(path).components().collect::<Vec<_>>();
    let mut directory = root.try_clone()?;
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("Invalid instruction file path.".into());
        };
        let name = CString::new(name.as_bytes())?;
        if index + 1 == parts.len() {
            return Ok(Some((directory, name)));
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if !create {
                return Ok(None);
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            sync(&directory)?;
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Err("Empty instruction file path.".into())
}
fn temporary(operation: Uuid, path: &str) -> String {
    let name = format!(
        ".intercat-instructions-{}",
        Uuid::new_v5(&operation, path.as_bytes())
    );
    Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .join(name)
        .to_string_lossy()
        .into_owned()
}
fn allowed(frozen: &Frozen, operation: Uuid, include_temporary: bool) -> BTreeSet<String> {
    let mut allowed = frozen
        .before
        .0
        .keys()
        .chain(frozen.after.0.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    if include_temporary {
        allowed.extend(
            allowed
                .iter()
                .map(|path| temporary(operation, path))
                .collect::<Vec<_>>(),
        );
    }
    allowed
}

pub async fn apply(branch: &Branch) -> Result<()> {
    let snapshot = branch
        .instructions
        .as_ref()
        .ok_or("Branch instructions were not frozen.")?;
    let frozen = load(&Repository::open(&branch.workspace.path)?, snapshot)?;
    crate::source_snapshot::verify_paths(
        &branch.workspace.path,
        &allowed(&frozen, branch.request.op_id, true),
    )
    .await?;
    let actual = Files::capture(&branch.workspace.path)?;
    for path in actual
        .0
        .keys()
        .chain(frozen.before.0.keys())
        .chain(frozen.after.0.keys())
    {
        let value = actual.0.get(path);
        if value != frozen.before.0.get(path) && value != frozen.after.0.get(path) {
            return Err("The branch instruction files changed before activation.".into());
        }
    }
    let root = open_root(&branch.workspace.path)?;
    for path in allowed(&frozen, branch.request.op_id, false) {
        let target = frozen.after.0.get(&path);
        let Some((directory, name)) = parent(&root, &path, target.is_some())? else {
            continue;
        };
        let temporary = CString::new(
            Path::new(&temporary(branch.request.op_id, &path))
                .file_name()
                .unwrap()
                .as_bytes(),
        )?;
        // This deterministic staging name belongs only to this frozen action.
        // It is also recognized before retrying a partially applied snapshot.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
        {
            return Err(std::io::Error::last_os_error().into());
        }
        sync(&directory)?;
        if actual.0.get(&path) == target {
            continue;
        }
        if let Some(target) = target {
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            if unsafe { libc::fchmod(file.as_raw_fd(), (target.mode & 0o777) as libc::mode_t) } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            file.write_all(target.text.as_bytes())?;
            sync(&file)?;
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        } else if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
        {
            return Err(std::io::Error::last_os_error().into());
        }
        sync(&directory)?;
    }
    verify(branch).await
}

pub async fn verify(branch: &Branch) -> Result<()> {
    let snapshot = branch
        .instructions
        .as_ref()
        .ok_or("Branch instructions were not frozen.")?;
    let frozen = load(&Repository::open(&branch.workspace.path)?, snapshot)?;
    if Files::capture(&branch.workspace.path)? != frozen.after {
        return Err("The chosen branch instructions changed before activation.".into());
    }
    crate::source_snapshot::verify_paths(
        &branch.workspace.path,
        &allowed(&frozen, branch.request.op_id, false),
    )
    .await
}

pub fn initial(
    workspace: &Path,
    snapshot: &InstructionSnapshot,
) -> Result<crate::instructions::Instructions> {
    load(&Repository::open(workspace)?, snapshot)?
        .after
        .instructions()
}
