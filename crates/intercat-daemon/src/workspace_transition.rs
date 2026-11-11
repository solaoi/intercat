//! Convert both sides of a worker file change in private checkouts. Project
//! filters never receive write access to the worker, its index, or its HEAD.
use crate::{
    Result,
    checkout::{Checkout, Configuration},
    preparation::Workspace,
    tree_transition::Image,
};
use intercat_git::{ObjectId, Repository, WorktreeSource};
use std::path::Path;

pub(crate) struct Prepared {
    before: Image,
    after: Image,
    _directory: tempfile::TempDir,
}
impl Prepared {
    pub(crate) fn apply(&self, root: &Path) -> Result<usize> {
        crate::tree_transition::apply(root, &self.before, &self.after)
    }
}

pub(crate) async fn prepare(
    workspace: &Workspace,
    before: ObjectId,
    after: ObjectId,
) -> Result<Prepared> {
    let source = WorktreeSource {
        root: workspace.path.canonicalize()?,
        common_dir: Repository::open(&workspace.path)?.common_directory()?,
        head: before,
    };
    let mut configuration = Configuration::capture(&source)?;
    configuration.denied_roots = workspace.denied_roots.clone();
    let path = std::env::var("PATH")?;
    let toolchain = crate::checkout::toolchain_paths(&path, &source.root)?;
    let directory = tempfile::Builder::new()
        .prefix("intercat-file-transition-")
        .tempdir()?;
    let root = directory.path().canonicalize()?;
    let (_cancel, cancel) = tokio::sync::watch::channel(false);
    let mut images = Vec::new();
    for (name, commit) in [("before", before), ("after", after)] {
        let checkout = Checkout::create(
            &root.join(name),
            source.clone(),
            commit,
            configuration.clone(),
            path.clone().into(),
            toolchain.clone(),
            cancel.clone(),
        )
        .await?;
        let tree = Repository::open(&source.root)?.tree_id(commit)?;
        checkout.deploy(tree, cancel.clone()).await?;
        let workdir = checkout.workdir.clone();
        images.push(
            tokio::task::spawn_blocking(move || {
                Image::capture(&Repository::open(&workdir)?, tree, &workdir)
            })
            .await??,
        );
    }
    let after = images.pop().ok_or("Missing converted after image.")?;
    let before = images.pop().ok_or("Missing converted before image.")?;
    Ok(Prepared {
        before,
        after,
        _directory: directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, process::Command};

    fn git(path: &Path, args: &[&str]) -> Result<()> {
        let result = Command::new("/usr/bin/git")
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).to_string().into());
        }
        Ok(())
    }
    fn payload(storage: &Path, bytes: &[u8]) -> Result<String> {
        let oid = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = storage
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, bytes)?;
        Ok(format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            bytes.len()
        ))
    }
    fn commit(repo: &Repository, base: ObjectId, files: &[(&str, &[u8])]) -> Result<ObjectId> {
        let files = files
            .iter()
            .map(|(path, bytes)| Ok((*path, repo.write_blob(bytes)?)))
            .collect::<Result<Vec<_>>>()?;
        repo.checkpoint_commit(repo.write_tree(&files)?, base, "Dummy converted update")
    }

    #[tokio::test]
    #[ignore = "native Git filters and LFS; requires sandbox-exec"]
    async fn converted_inheritance_preserves_index_head_other_files_and_partial_replay()
    -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let path = root.path().join("worker");
        fs::create_dir(&path)?;
        gix::init(&path)?;
        let repo = Repository::open(&path)?;
        let base = repo.empty_starting_point()?;
        let lfs = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tools/git-lfs-3.8.1/git-lfs")
            .canonicalize()?;
        for (key, command) in [
            ("filter.lfs.clean", "clean -- %f"),
            ("filter.lfs.smudge", "smudge -- %f"),
            ("filter.lfs.process", "filter-process"),
        ] {
            git(
                &path,
                &["config", key, &format!("'{}' {command}", lfs.display())],
            )?;
        }
        git(&path, &["config", "filter.lfs.required", "true"])?;
        git(&path, &["config", "filter.upper.clean", "tr a-z A-Z"])?;
        // A smudge filter is project code. It must not read the original
        // workspace or modify even an unrelated file in that workspace.
        git(
            &path,
            &[
                "config",
                "filter.upper.smudge",
                &format!(
                    "if cat '{0}/untouched' >/dev/null 2>&1; then exit 8; fi; if (echo bad > '{0}/untouched') 2>/dev/null; then exit 9; fi; tr A-Z a-z",
                    path.display()
                ),
            ],
        )?;
        git(&path, &["config", "filter.upper.required", "true"])?;
        let old_bytes = vec![0x01; 2 * 1024 * 1024 + 17];
        let new_bytes = vec![0x02; 3 * 1024 * 1024 + 7];
        let storage = path.join(".git/lfs");
        let old_pointer = payload(&storage, &old_bytes)?;
        let new_pointer = payload(&storage, &new_bytes)?;
        let attrs = b"*.upper filter=upper\n*.crlf text eol=crlf\n*.lfs filter=lfs -text\n";
        let before = commit(
            &repo,
            base,
            &[
                (".gitattributes", attrs),
                ("text.upper", b"OLD\n"),
                ("lines.crlf", b"one\ntwo\n"),
                ("asset.lfs", old_pointer.as_bytes()),
                ("gone", b"remove me"),
            ],
        )?;
        let after = commit(
            &repo,
            before,
            &[
                (".gitattributes", attrs),
                ("text.upper", b"NEW\n"),
                ("lines.crlf", b"one\nthree\n"),
                ("asset.lfs", new_pointer.as_bytes()),
                ("added", b"new file"),
            ],
        )?;
        for (name, sha) in [("before", before), ("after", after)] {
            repo.compare_and_swap(&format!("refs/intercat/dummy/{name}"), None, sha)?;
        }
        git(&path, &["read-tree", &before.to_string()])?;
        fs::write(path.join(".gitattributes"), attrs)?;
        fs::write(path.join("text.upper"), b"old\n")?;
        fs::write(path.join("lines.crlf"), b"one\r\ntwo\r\n")?;
        fs::write(path.join("asset.lfs"), &old_bytes)?;
        fs::write(path.join("gone"), b"remove me")?;
        fs::write(path.join("untouched"), b"dummy separate edit")?;
        let head = fs::read(path.join(".git/HEAD"))?;
        let index = fs::read(path.join(".git/index"))?;
        let workspace = Workspace {
            denied_roots: vec![],
            attachment_roots: vec![],
            track_id: "dummy".into(),
            path: path.clone(),
            reference: "refs/intercat/dummy/tracks/A".into(),
            base: base.to_string(),
            state: "ready".into(),
            error: None,
        };
        let files = prepare(&workspace, before, after).await?;
        // Refuse all writes if just one tracked file changed after planning.
        fs::write(path.join("text.upper"), b"separate edit\n")?;
        assert!(files.apply(&path).is_err());
        assert_eq!(fs::read(path.join("asset.lfs"))?, old_bytes);
        assert!(path.join("gone").is_file());
        fs::write(path.join("text.upper"), b"old\n")?;
        // A crash may leave a mixture of before and after bytes.
        fs::write(path.join("lines.crlf"), b"one\r\nthree\r\n")?;
        files.apply(&path)?;
        files.apply(&path)?;
        assert_eq!(fs::read(path.join("text.upper"))?, b"new\n");
        assert_eq!(fs::read(path.join("lines.crlf"))?, b"one\r\nthree\r\n");
        assert_eq!(fs::read(path.join("asset.lfs"))?, new_bytes);
        assert_eq!(fs::read(path.join("added"))?, b"new file");
        assert!(!path.join("gone").exists());
        assert_eq!(fs::read(path.join("untouched"))?, b"dummy separate edit");
        assert_eq!(fs::read(path.join(".git/HEAD"))?, head);
        assert_eq!(fs::read(path.join(".git/index"))?, index);
        assert_eq!(
            repo.reference_target("refs/intercat/dummy/before")?,
            Some(before)
        );
        Ok(())
    }
}
