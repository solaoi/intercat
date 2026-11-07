//! Copy untracked hook helpers without replacing hooks in the candidate tree.
use crate::{
    Result,
    checkout::{Checkout, shell_quote},
};
use intercat_git::{ObjectId, Repository, WorktreeSource};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

pub struct Prepared {
    pub directory: PathBuf,
    pub copied: Vec<PathBuf>,
}

fn safe_target(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("Unsafe hook path.".into());
        };
        current.push(part);
        if current != root.join(relative) {
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current)?
                }
                _ => return Err("A hook destination contains a symlink or file.".into()),
            }
        }
    }
    if fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err("A hook destination is a symlink.".into());
    }
    Ok(current)
}

pub fn prepare(
    checkout: &Checkout,
    source: &WorktreeSource,
    tree: ObjectId,
    excluded: Option<&Path>,
) -> Result<Prepared> {
    checkout.verify_storage()?;
    let source_hook = &checkout.configuration.hooks_directory;
    let (source_root, destination_root, git_metadata) = if source_hook
        .starts_with(&source.common_dir)
    {
        let relative = source_hook.strip_prefix(&source.common_dir)?;
        let first = relative
            .components()
            .next()
            .ok_or("Hooks cannot be the entire Git directory.")?;
        if !matches!(first,Component::Normal(name) if !["objects","refs","logs","worktrees","modules"].iter().any(|reserved|name==*reserved))
        {
            return Err("Hooks need files outside the hook directory.".into());
        }
        (
            source.common_dir.clone(),
            checkout.workdir.join(".git"),
            true,
        )
    } else if source_hook.starts_with(&source.root) {
        (source.root.clone(), checkout.workdir.clone(), false)
    } else {
        return Err("Hooks need files outside the repo.".into());
    };
    let relative = source_hook.strip_prefix(&source_root)?;
    let real_hooks = safe_target(&destination_root, relative)?;
    if let Ok(meta) = fs::symlink_metadata(source_hook)
        && (!meta.is_dir() || meta.file_type().is_symlink())
    {
        return Err("Hooks need files outside the repo.".into());
    }
    let target_repo = Repository::open(&checkout.workdir)?;
    let target_files = target_repo.tree_files(tree)?;
    let source_files: BTreeSet<_> = Repository::open(&source.root)?
        .tracked_paths()?
        .into_iter()
        .collect();
    let mut copied = Vec::new();
    if source_hook.is_dir() {
        for entry in walkdir::WalkDir::new(source_hook)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.path() == source_hook
                    || (entry.file_name() != ".git"
                        && entry.file_name() != "node_modules"
                        && !intercat_git::secret_path(&entry.file_name().to_string_lossy()))
            })
        {
            let entry = entry?;
            if entry.depth() > 32 {
                return Err("Hook helpers are nested too deeply.".into());
            }
            if entry.path() == source_hook || entry.file_type().is_dir() {
                continue;
            }
            if copied.len() >= 4096 {
                return Err("Too many hook helper files.".into());
            }
            let relative = entry.path().strip_prefix(&source_root)?;
            if !git_metadata {
                let key = relative.to_str().ok_or("Non-UTF8 hook path.")?;
                // A deleted tracked hook stays deleted. A new tracked version
                // wins over an old untracked helper with the same path.
                if source_files.contains(key) || target_files.contains_key(key) {
                    continue;
                }
            }
            let canonical = entry.path().canonicalize()?;
            if excluded == Some(canonical.as_path()) {
                continue;
            }
            if !(canonical.starts_with(&source.root) || canonical.starts_with(source_hook))
                || canonical.starts_with(&source.common_dir) && !canonical.starts_with(source_hook)
                || !canonical.is_file()
            {
                return Err("Hooks need files outside the repo.".into());
            }
            let metadata = fs::metadata(&canonical)?;
            if metadata.len() > 4 * 1024 * 1024 {
                return Err("A hook helper exceeds 4 MiB.".into());
            }
            let destination = safe_target(&destination_root, relative)?;
            fs::write(&destination, fs::read(&canonical)?)?;
            fs::set_permissions(
                &destination,
                fs::Permissions::from_mode(metadata.permissions().mode() & 0o777),
            )?;
            copied.push(destination);
        }
    }
    let directory = checkout
        .root
        .join("control")
        .join(format!("hooks-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&directory)?;
    for name in [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
    ] {
        let hook = real_hooks.join(name);
        let Ok(meta) = fs::metadata(&hook) else {
            continue;
        };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let resolved = hook.canonicalize()?;
        if !resolved.starts_with(&checkout.workdir) {
            return Err("Hooks need files outside the repo.".into());
        }
        let mut script = format!("#!/bin/sh\nprintf '%s\\n' 'Intercat hook: {name}' >&2\n");
        for file in &copied {
            script.push_str(&format!("test -f {} || {{ printf '%s\\n' 'A copied hook helper is missing' >&2; exit 1; }}\n",shell_quote(file.as_os_str())?));
        }
        script.push_str(&format!("exec {} \"$@\"\n", shell_quote(hook.as_os_str())?));
        let wrapper = directory.join(name);
        fs::write(&wrapper, script)?;
        fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700))?;
    }
    Ok(Prepared { directory, copied })
}
