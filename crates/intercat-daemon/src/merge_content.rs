//! Frozen, converted conflict sides live with the original private candidate,
//! outside the model's writable workspace. SQLite never receives their bytes.
use crate::{Result, checkout::Checkout, merge_workspace::Input};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::sync::watch;

const TEXT_LIMIT: u64 = 1024 * 1024;
pub(crate) fn text(bytes: Vec<u8>) -> Option<String> {
    if bytes
        .iter()
        .any(|byte| *byte < 0x20 && !matches!(*byte, b'\t' | b'\r' | b'\n'))
    {
        None
    } else {
        String::from_utf8(bytes).ok()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Version {
    pub object: String,
    pub mode: u16,
    pub text: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileSides {
    pub path: String,
    pub base: Option<Version>,
    pub target: Option<Version>,
    pub incoming: Option<Version>,
}
impl FileSides {
    pub fn editable(&self) -> bool {
        self.base
            .as_ref()
            .is_none_or(|v| matches!(v.mode, 0o100644 | 0o100755) && v.text.is_some())
            && [&self.target, &self.incoming].iter().all(|side| {
                side.as_ref()
                    .is_some_and(|v| matches!(v.mode, 0o100644 | 0o100755) && v.text.is_some())
            })
    }
}
fn directory(original: &Path, input: &Input) -> Result<PathBuf> {
    Ok(original
        .parent()
        .ok_or("Missing private candidate root.")?
        .join("control")
        .join(format!("merge-{}", input.marker)))
}
fn name(path: &str) -> String {
    let hex: String = Sha256::digest(path.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{hex}.json")
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    crate::restoration::sync_directory(path.parent().ok_or("Missing conflict cache directory.")?)
}
pub(crate) fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("Conflict content is not a bounded regular file.".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("Conflict content changed while being read.".into());
    }
    Ok(bytes)
}
pub fn load(original: &Path, input: &Input, path: &str) -> Result<FileSides> {
    let directory = directory(original, input)?;
    if !paths(original, input)?.iter().any(|item| item == path) {
        return Err("Choose a path from this merge's conflict list.".into());
    }
    let sides: FileSides =
        serde_json::from_slice(&read(&directory.join(name(path)), 24 * TEXT_LIMIT)?)?;
    if sides.path != path {
        return Err("The frozen conflict path changed.".into());
    }
    Ok(sides)
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    input: Input,
    paths: Vec<String>,
}
pub fn paths(original: &Path, input: &Input) -> Result<Vec<String>> {
    let frozen: Manifest = serde_json::from_slice(&read(
        &directory(original, input)?.join("input.json"),
        4 * TEXT_LIMIT,
    )?)?;
    if &frozen.input != input {
        return Err("The frozen conflict sides belong to another merge.".into());
    }
    Ok(frozen.paths)
}
fn version(
    file: Option<intercat_git::SavedFile>,
    temporary: &[u8],
    root: &Path,
) -> Result<Option<Version>> {
    let Some(file) = file else {
        if temporary != b"." {
            return Err("An absent conflict side unexpectedly has content.".into());
        }
        return Ok(None);
    };
    let temporary = std::str::from_utf8(temporary)?;
    if !temporary.starts_with(".merge_file_") || temporary.contains('/') || temporary.contains('\\')
    {
        return Err("Invalid temporary conflict file.".into());
    }
    let path = root.join(temporary);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Conflict conversion did not produce a regular file.".into());
    }
    let text = if metadata.len() <= TEXT_LIMIT && matches!(file.mode, 0o100644 | 0o100755) {
        let bytes = read(&path, TEXT_LIMIT)?;
        text(bytes)
    } else {
        None
    };
    std::fs::remove_file(path)?;
    Ok(Some(Version {
        object: file.object.to_string(),
        mode: file.mode,
        text,
    }))
}

pub async fn prepare(
    checkout: &Checkout,
    original: &Path,
    input: &Input,
    cancel: watch::Receiver<bool>,
) -> Result<()> {
    let stages = Repository::open(&checkout.workdir)?
        .resolution_stages(&checkout.workdir.join(".git/index"))?;
    let paths = stages.iter().map(|stage| stage.path.clone()).collect();
    let output = checkout
        .git(
            &[
                "checkout-index".into(),
                "--all".into(),
                "--stage=all".into(),
                "--temp".into(),
                "-z".into(),
            ],
            true,
            cancel,
        )
        .await?
        .require_success("Read conflict sides")?;
    if output.truncated {
        return Err("The conflict file list exceeds the display limit.".into());
    }
    let directory = directory(original, input)?;
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let mut records = std::collections::BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let (files, path) = record.split_at(
            record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("Invalid converted conflict listing.")?,
        );
        let path = std::str::from_utf8(&path[1..])?.to_owned();
        let files = files.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if files.len() != 3 || records.insert(path, files).is_some() {
            return Err("Invalid converted conflict sides.".into());
        }
    }
    if records.len() != stages.len() {
        return Err("The conflict index changed during conversion.".into());
    }
    for stage in stages {
        let files = records
            .remove(&stage.path)
            .ok_or("Missing converted conflict path.")?;
        let sides = FileSides {
            path: stage.path.clone(),
            base: version(stage.base, files[0], &checkout.workdir)?,
            target: version(stage.target, files[1], &checkout.workdir)?,
            incoming: version(stage.incoming, files[2], &checkout.workdir)?,
        };
        if sides.editable() {
            let target = sides.target.as_ref().unwrap();
            let incoming = sides.incoming.as_ref().unwrap();
            let text = intercat_git::merge::text(
                sides
                    .base
                    .as_ref()
                    .and_then(|side| side.text.as_deref())
                    .unwrap_or("")
                    .as_bytes(),
                target.text.as_deref().unwrap().as_bytes(),
                incoming.text.as_deref().unwrap().as_bytes(),
                &input.marker.to_string(),
            );
            crate::restoration::write_file(&checkout.workdir, &sides.path, target.mode, &text)?;
        }
        write(
            &directory.join(name(&sides.path)),
            &serde_json::to_vec(&sides)?,
        )?;
    }
    write(
        &directory.join("input.json"),
        &serde_json::to_vec(&Manifest {
            input: input.clone(),
            paths,
        })?,
    )?;
    File::open(&directory)?.sync_all()?;
    Ok(())
}
