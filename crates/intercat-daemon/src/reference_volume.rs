//! Private, restartable, read-only images of frozen dependency checkpoints.
use crate::{Result, readonly_snapshot::Snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    pub track: String,
    pub checkpoint: String,
    pub number: u64,
    pub repository: PathBuf,
}
impl Source {
    pub fn name(&self) -> Result<String> {
        let track: Uuid = self.track.parse()?;
        if self.number < 1 {
            return Err("Invalid dependency checkpoint number.".into());
        }
        Ok(format!("{track}-{}", self.number))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Recipe {
    pub input: Uuid,
    pub track: Uuid,
    pub sources: Vec<Source>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sealed {
    recipe: Recipe,
    sha256: String,
}

pub(crate) struct Volume {
    directory: PathBuf,
    pub root: PathBuf,
    pub recipe: Recipe,
}

fn private_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir() || info.uid() != unsafe { libc::geteuid() } || info.mode() & 0o077 != 0 {
        return Err("Dependency references need a private, owned directory.".into());
    }
    Ok(())
}
fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.nlink() != 1
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o077 != 0
        || info.len() > limit
    {
        return Err("Dependency reference metadata changed.".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("Dependency reference metadata is too large.".into());
    }
    Ok(bytes)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing reference directory")?)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn image_hash(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.nlink() != 1
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o077 != 0
        || info.len() > 1024 * 1024 * 1024
    {
        return Err("The dependency image changed.".into());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
async fn run(command: &mut Command) -> Result<Vec<u8>> {
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output()).await??;
    if !output.status.success() {
        return Err(format!(
            "Dependency volume command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output.stdout)
}
async fn mounted_image(root: &Path) -> Result<Option<PathBuf>> {
    let xml = run(Command::new("/usr/bin/hdiutil").args(["info", "-plist"])).await?;
    let mut command = Command::new("/usr/bin/plutil");
    command
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("Missing property list input")?
        .write_all(&xml)
        .await?;
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await??;
    if !output.status.success() {
        return Err("Cannot inspect dependency mounts.".into());
    }
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let images = info["images"]
        .as_array()
        .ok_or("Missing dependency mount inventory.")?;
    for image in images {
        if image["system-entities"].as_array().is_some_and(|entities| {
            entities.iter().any(|entity| {
                entity["mount-point"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == root)
            })
        }) {
            return Ok(Some(PathBuf::from(
                image["image-path"]
                    .as_str()
                    .ok_or("Unknown dependency image.")?,
            )));
        }
    }
    Ok(None)
}
fn readonly_mount(root: &Path) -> Result<()> {
    let path = CString::new(root.as_os_str().as_bytes())?;
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut info) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if info.f_flags & libc::MNT_RDONLY as u32 == 0 {
        return Err("Dependency volume is not read-only.".into());
    }
    Ok(())
}

impl Volume {
    fn recipes(parent: &Path, track: Uuid) -> Result<Vec<Recipe>> {
        let directory = parent.join(track.to_string());
        if !directory.try_exists()? {
            return Ok(vec![]);
        }
        private_directory(parent)?;
        private_directory(&directory)?;
        let mut recipes = vec![];
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let input: Uuid = entry
                .file_name()
                .to_str()
                .ok_or("Invalid dependency reference name.")?
                .parse()?;
            private_directory(&entry.path())?;
            let path = entry.path().join("recipe.json");
            if !path.try_exists()? {
                continue;
            }
            let recipe: Recipe = serde_json::from_slice(&read_private(&path, 1024 * 1024)?)?;
            if recipe.input != input || recipe.track != track {
                return Err("Dependency reference identity changed.".into());
            }
            recipes.push(recipe);
        }
        Ok(recipes)
    }
    pub async fn mount_saved(parent: &Path, recipes: Vec<Recipe>) -> Result<()> {
        for recipe in recipes {
            let sealed = parent
                .join(recipe.track.to_string())
                .join(recipe.input.to_string())
                .join("sealed.json");
            // Queued references and interrupted image construction are resumed
            // only when their input is prepared, never by opening a worker.
            if sealed.try_exists()? {
                Self::open(parent, recipe).await?;
            }
        }
        Ok(())
    }
    /// The owner calls this only after confirming its worker processes stopped.
    /// A busy mount is retained for a later cleanup; detach is never forced.
    pub async fn unmount_track(
        parent: &Path,
        track: Uuid,
        retained: std::collections::BTreeSet<Uuid>,
    ) -> Result<()> {
        if !parent.try_exists()? {
            return Ok(());
        }
        let parent = parent.canonicalize()?;
        for recipe in Self::recipes(&parent, track)? {
            let directory = parent
                .join(track.to_string())
                .join(recipe.input.to_string());
            let volume = Self {
                root: directory.join("mounted"),
                directory,
                recipe,
            };
            volume.unmount().await?;
            if !retained.contains(&volume.recipe.input) {
                fs::remove_dir_all(&volume.directory)?;
            }
        }
        Ok(())
    }
    pub async fn open(parent: &Path, recipe: Recipe) -> Result<Self> {
        if recipe.sources.is_empty() {
            return Err("A dependency volume needs a saved checkpoint.".into());
        }
        for source in &recipe.sources {
            source.name()?;
            source.checkpoint.parse::<intercat_git::ObjectId>()?;
        }
        private_directory(parent)?;
        let parent = parent.canonicalize()?;
        let track = parent.join(recipe.track.to_string());
        private_directory(&track)?;
        let directory = track.join(recipe.input.to_string());
        private_directory(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("volume.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("This dependency volume is already being prepared.".into());
        }
        let intent = directory.join("recipe.json");
        if intent.try_exists()? {
            let saved: Recipe = serde_json::from_slice(&read_private(&intent, 1024 * 1024)?)?;
            if saved != recipe {
                return Err("The dependency checkpoints changed.".into());
            }
        } else {
            write_private(&intent, &serde_json::to_vec(&recipe)?)?;
        }
        let image = directory.join("reference.dmg");
        let seal = directory.join("sealed.json");
        if !seal.try_exists()? {
            let source = recipe.sources.clone();
            let staging = directory.clone();
            let snapshot = tokio::task::spawn_blocking(move || -> Result<Snapshot> {
                let mut snapshot = Snapshot::new(&staging.join("staging"))?;
                for source in source {
                    snapshot.export(
                        &source.repository,
                        source.checkpoint.parse()?,
                        &source.name()?,
                    )?;
                }
                snapshot.freeze()?;
                Ok(snapshot)
            })
            .await??;
            // A cancelled helper must never resume writing a later attempt's
            // file. Each build therefore has its own unpublished pathname.
            let temporary = directory.join(format!("building-{}.dmg", Uuid::now_v7()));
            let created = run(Command::new("/usr/bin/hdiutil")
                .args([
                    "create",
                    "-quiet",
                    "-fs",
                    "HFSX",
                    "-format",
                    "UDRO",
                    "-srcfolder",
                ])
                .arg(snapshot.path())
                .args(["-volname", "Intercat reference"])
                .arg(&temporary))
            .await;
            if let Err(error) = created {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            File::open(&temporary)?.sync_all()?;
            fs::rename(temporary, &image)?;
            let digest_path = image.clone();
            let sha256 = tokio::task::spawn_blocking(move || image_hash(&digest_path)).await??;
            write_private(
                &seal,
                &serde_json::to_vec(&Sealed {
                    recipe: recipe.clone(),
                    sha256,
                })?,
            )?;
        }
        let sealed: Sealed = serde_json::from_slice(&read_private(&seal, 1024 * 1024)?)?;
        if sealed.recipe != recipe {
            return Err("The sealed dependency checkpoints changed.".into());
        }
        let digest_path = image.clone();
        if tokio::task::spawn_blocking(move || image_hash(&digest_path)).await?? != sealed.sha256 {
            return Err("The saved dependency image changed.".into());
        }
        let root = directory.join("mounted");
        if let Some(mounted) = mounted_image(&root).await? {
            if mounted != image {
                return Err("Another image occupies the dependency mount.".into());
            }
        } else {
            private_directory(&root)?;
            if fs::read_dir(&root)?.next().is_some() {
                return Err("The dependency mount is not empty.".into());
            }
            run(Command::new("/usr/bin/hdiutil")
                .args(["attach", "-quiet", "-readonly", "-nobrowse", "-mountpoint"])
                .arg(&root)
                .arg(&image))
            .await?;
            if mounted_image(&root).await?.as_ref() != Some(&image) {
                return Err("Dependency mount identity could not be confirmed.".into());
            }
        }
        readonly_mount(&root)?;
        drop(lock);
        Ok(Self {
            directory,
            root,
            recipe,
        })
    }

    pub async fn unmount(&self) -> Result<()> {
        let image = self.directory.join("reference.dmg");
        match mounted_image(&self.root).await? {
            None => return Ok(()),
            Some(mounted) if mounted == image => {}
            Some(_) => return Err("Another image occupies the dependency mount.".into()),
        }
        run(Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet"])
            .arg(&self.root))
        .await?;
        if mounted_image(&self.root).await?.is_some() {
            return Err("Dependency volume is still mounted.".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn saved_reference_volume_is_readonly_and_reopens_without_the_source_checkout()
    -> Result<()> {
        let temporary = tempfile::tempdir_in("/private/tmp")?;
        let source = temporary.path().join("source");
        fs::create_dir(&source)?;
        gix::init(&source)?;
        let repo = intercat_git::Repository::open(&source)?;
        let empty = repo.empty_starting_point()?;
        let saved = repo.write_blob(b"saved dummy data\n")?;
        let secret = repo.write_blob(b"dummy secret excluded\n")?;
        let commit = repo.checkpoint_commit(
            repo.write_tree(&[("saved.txt", saved), (".env", secret)])?,
            empty,
            "Dummy saved reference",
        )?;
        let recipe = Recipe {
            input: Uuid::now_v7(),
            track: Uuid::now_v7(),
            sources: vec![Source {
                track: Uuid::now_v7().to_string(),
                checkpoint: commit.to_string(),
                number: 1,
                repository: source.clone(),
            }],
        };
        let parent = temporary.path().join("references");
        let volume = Volume::open(&parent, recipe.clone()).await?;
        let result = async {
            let files = volume.root.join(recipe.sources[0].name()?);
            assert_eq!(fs::read(files.join("saved.txt"))?, b"saved dummy data\n");
            assert!(!files.join(".env").exists());
            assert!(!files.join(".git").exists());
            assert!(fs::write(files.join("saved.txt"), b"changed").is_err());
            assert!(
                fs::set_permissions(files.join("saved.txt"), fs::Permissions::from_mode(0o600))
                    .is_err()
            );
            assert!(
                fs::hard_link(files.join("saved.txt"), temporary.path().join("alias")).is_err()
            );
            let reopened = Volume::open(&parent, recipe.clone()).await?;
            assert_eq!(reopened.root, volume.root);
            volume.unmount().await?;
            fs::remove_dir_all(&source)?;
            let reopened = Volume::open(&parent, recipe.clone()).await?;
            assert_eq!(
                fs::read(
                    reopened
                        .root
                        .join(recipe.sources[0].name()?)
                        .join("saved.txt")
                )?,
                b"saved dummy data\n"
            );
            reopened.unmount().await?;
            let mut changed = recipe.clone();
            changed.sources[0].number += 1;
            assert!(Volume::open(&parent, changed).await.is_err());
            fs::write(volume.directory.join("reference.dmg"), b"changed image")?;
            assert!(Volume::open(&parent, recipe).await.is_err());
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        volume.unmount().await?;
        result
    }
}
