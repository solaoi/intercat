//! Private, complete Git transport data. Hook execution uses another checkout.
use crate::{
    Result,
    push_review::{Member, Review},
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Intent {
    review: Review,
    folder_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Prepared {
    commits: Vec<String>,
    shallow: Vec<String>,
}
pub struct Snapshot {
    pub root: PathBuf,
    pub git_directory: PathBuf,
    pub commits: Vec<String>,
    pub shallow: Vec<String>,
    member: Member,
}
fn sync(file: &fs::File) -> Result<()> {
    // SAFETY: this owned descriptor remains open for the complete call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn rename_exclusive(source: &Path, target: &Path) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both paths are terminated, owned strings for the complete call.
    if unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn write_new(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(bytes)?;
    sync(file.as_file())?;
    match rename_exclusive(file.path(), &root.join(name)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if crate::private_input::read(root, &[name], bytes.len() as u64)? != bytes {
                return Err("The saved Push transport record changed.".into());
            }
        }
        Err(error) => return Err(error.into()),
    }
    sync(&fs::File::open(root)?)
}
fn private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("The Push transport directory is no longer private.".into());
    }
    Ok(())
}
impl Snapshot {
    pub fn prepare(
        review: &Review,
        folder_id: &str,
        root: &Path,
        interrupted: &AtomicBool,
    ) -> Result<Self> {
        review.validate()?;
        let member = review
            .members
            .iter()
            .find(|member| member.folder.id == folder_id)
            .ok_or("The folder is not in this Push review.")?
            .clone();
        let root = crate::physical_path::resolve(root)?;
        if root.starts_with(&member.folder.root) || root.starts_with(&member.folder.common_dir) {
            return Err("Push transport data must be outside the source repository.".into());
        }
        let intent = Intent {
            review: review.clone(),
            folder_id: folder_id.into(),
        };
        if !root.exists() {
            let parent = root.parent().ok_or("Missing Push transport parent.")?;
            let staging = tempfile::Builder::new()
                .prefix("push-preparing-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(parent)?;
            write_new(staging.path(), "intent.json", &serde_json::to_vec(&intent)?)?;
            match rename_exclusive(staging.path(), &root) {
                Ok(()) => sync(&fs::File::open(parent)?)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let saved = crate::private_input::read(&root, &["intent.json"], 1024 * 1024)?;
        if serde_json::from_slice::<serde_json::Value>(&saved)? != serde_json::to_value(&intent)? {
            return Err("The transport directory belongs to another Push review.".into());
        }
        let git_directory = root.join("git");
        if !git_directory.exists() {
            Repository::init_isolated(&git_directory)?;
        }
        private_directory(&git_directory)?;
        private_directory(&git_directory.join("objects"))?;
        private_directory(&git_directory.join("refs"))?;
        if git_directory.join("objects/info/alternates").exists()
            || git_directory.join("commondir").exists()
        {
            return Err("Push transport data must not share a Git object store.".into());
        }
        let repo = Repository::open(&git_directory)?;
        let prepared = if root.join("prepared.json").exists() {
            serde_json::from_slice::<Prepared>(&crate::private_input::read(
                &root,
                &["prepared.json"],
                64 * 1024 * 1024,
            )?)?
        } else {
            let source = Repository::open(&member.folder.root)?;
            if source.common_directory()? != member.folder.common_dir {
                return Err("The Push source repository changed.".into());
            }
            let history =
                repo.import_push_history(&source, member.target.sha.parse()?, interrupted)?;
            let prepared = Prepared {
                commits: history.commits.iter().map(ToString::to_string).collect(),
                shallow: history.shallow.iter().map(ToString::to_string).collect(),
            };
            write_new(&root, "prepared.json", &serde_json::to_vec(&prepared)?)?;
            prepared
        };
        let reference = "refs/heads/intercat/send";
        repo.commit_details(member.target.sha.parse()?)?;
        repo.compare_and_swap(reference, None, member.target.sha.parse()?)?;
        repo.sync_ref(reference)?;
        Ok(Self {
            root,
            git_directory,
            commits: prepared.commits,
            shallow: prepared.shallow,
            member,
        })
    }
    pub fn member(&self) -> &Member {
        &self.member
    }
}
