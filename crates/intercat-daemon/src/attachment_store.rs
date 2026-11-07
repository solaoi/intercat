//! Immutable private originals and verified, non-executable workspace copies.
use crate::{Result, preparation::Workspace, secrets::Key};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub const FILE_LIMIT: usize = 25 * 1024 * 1024;
pub const IMAGE_LIMIT: usize = 10 * 1024 * 1024;
pub const INPUT_LIMIT: u64 = 100 * 1024 * 1024;
pub const TEXT_LIMIT: usize = 2 * 1024 * 1024;
pub(crate) fn digest(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}
const MAGIC: &[u8; 8] = b"ICATATT1";
const TEXT_MAGIC: &[u8; 8] = b"ICATTXT1";
const HEADER_LIMIT: usize = 16384;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attachment {
    pub id: Uuid,
    pub name: String,
    pub byte_len: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Stored {
    pub attachment: Attachment,
    pub source: PathBuf,
    pub seal: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextHeader {
    stored: Stored,
    original_length: usize,
    original_sha256: String,
}
pub(crate) struct CopyPlan {
    pub workspace: Workspace,
    pub originals: Vec<Stored>,
    pub input_ids: Vec<Uuid>,
}
pub(crate) fn copy_name(attachment: &Attachment) -> String {
    let extension = Path::new(&attachment.name)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.len() <= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("txt");
    format!("{}.{}", attachment.id, extension)
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceContext {
    pub files: Vec<Reference>,
    pub directory: Option<PathBuf>,
}
#[derive(Clone)]
pub(crate) struct Store {
    pub root: PathBuf,
    pub key: Arc<Key>,
}

pub(crate) fn save(db: &mut Connection, stored: &Stored, masked: bool) -> Result<()> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let id = stored.attachment.id.to_string();
    let payload = serde_json::to_string(stored)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, previous)) = old {
        if kind != "attachment.import" || previous != payload {
            return Err("Attachment operation ID reused.".into());
        }
        return Ok(());
    }
    tx.execute(
        "INSERT INTO attachments VALUES(?1,?2)",
        params![id, payload],
    )?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'attachment.import',?2,'complete')",
        params![id, payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'attachment.saved',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(&stored.attachment)?
        ],
    )?;
    if masked {
        crate::editor_log::pasted_masked(&tx, &format!("masked-paste/{id}"))?;
    }
    tx.commit()?;
    Ok(())
}
pub(crate) fn get(db: &Connection, id: Uuid) -> Result<Stored> {
    let raw: String = db.query_row(
        "SELECT record FROM attachments WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    let stored: Stored = serde_json::from_str(&raw)?;
    if stored.attachment.id != id {
        return Err("The attachment record has a different identity.".into());
    }
    Ok(stored)
}
pub(crate) fn bind(db: &Connection, input: &str, ids: &[Uuid]) -> Result<()> {
    if ids.len() > 128 {
        return Err("Too many reference files in one request.".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for (ordinal, id) in ids.iter().enumerate() {
        if !seen.insert(*id) {
            return Err("A reference file was included twice.".into());
        }
        let stored = get(db, *id)?;
        if stored.attachment.byte_len > FILE_LIMIT as u64 {
            return Err("An attachment exceeds the file limit.".into());
        }
        total = total
            .checked_add(stored.attachment.byte_len)
            .ok_or("The reference sizes overflowed.")?;
        if total > INPUT_LIMIT {
            return Err("The reference files exceed 100 MiB in one request.".into());
        }
        db.execute(
            "INSERT INTO input_attachments VALUES(?1,?2,?3)",
            params![input, id.to_string(), ordinal as i64],
        )?;
    }
    Ok(())
}
pub(crate) fn records(db: &Connection, input: &str) -> Result<Vec<Stored>> {
    let mut stmt=db.prepare("SELECT a.record FROM input_attachments i JOIN attachments a ON a.id=i.attachment WHERE i.input=?1 ORDER BY i.ordinal")?;
    stmt.query_map([input], |r| r.get::<_, String>(0))?
        .map(|s| Ok(serde_json::from_str(&s?)?))
        .collect()
}
pub(crate) fn material(
    db: &Connection,
    input: &str,
    track: &str,
    session: &str,
) -> Result<Option<CopyPlan>> {
    let current = records(db, input)?;
    let current_ids = current.iter().map(|s| s.attachment.id).collect::<Vec<_>>();
    let mut ids = current_ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let inherited = crate::attachment_history::available(db, session)?;
    if !inherited.is_empty() {
        verify_session_owner(db, session, track)?;
    }
    ids.extend(inherited);
    let records = ids
        .into_iter()
        .map(|id| get(db, id))
        .collect::<Result<Vec<_>>>()?;
    if records.is_empty() {
        return Ok(None);
    }
    let valid:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1 AND state='placed' AND json_extract(decision,'$.trackId')=?2)",params![input,track],|r|r.get(0))?;
    if !valid {
        return Err("The attachment request changed before preparation.".into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [track],
        |r| r.get(0),
    )?;
    let workspace: Workspace = serde_json::from_str(&raw)?;
    if workspace.state != "ready" {
        return Err("The attachment workspace is not ready.".into());
    }
    Ok(Some(CopyPlan {
        workspace,
        originals: records,
        input_ids: current_ids,
    }))
}
pub(crate) fn frozen(
    db: &Connection,
    owner: Uuid,
    workspace: Workspace,
) -> Result<Option<CopyPlan>> {
    let ids = crate::attachment_history::frozen_ids(db, owner)?;
    if ids.is_empty() {
        return Ok(None);
    }
    let originals = ids
        .into_iter()
        .map(|id| get(db, id))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(CopyPlan {
        workspace,
        originals,
        input_ids: vec![],
    }))
}
pub(crate) fn session(db: &Connection, session: &str, track: &str) -> Result<Option<CopyPlan>> {
    let ids = crate::attachment_history::restorable(db, session)?;
    if ids.is_empty() {
        return Ok(None);
    }
    verify_session_owner(db, session, track)?;
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [track],
        |r| r.get(0),
    )?;
    let workspace = serde_json::from_str(&raw)?;
    let originals = ids
        .into_iter()
        .map(|id| get(db, id))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(CopyPlan {
        workspace,
        originals,
        input_ids: vec![],
    }))
}

fn verify_session_owner(db: &Connection, session: &str, track: &str) -> Result<()> {
    let valid:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2)",params![track,session],|r|r.get(0))?;
    if !valid {
        return Err("The attachment session belongs to another worker.".into());
    }
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn sync(file: &File) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn unchanged(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn open_directory(parent: &File, name: &std::ffi::CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Store {
    fn directory(&self, create: bool) -> Result<File> {
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&self.root) {
                Ok(()) => sync(&File::open(
                    self.root.parent().ok_or("Missing attachment parent.")?,
                )?)?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.root)?;
        let meta = file.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("The original attachment directory is not private.".into());
        }
        Ok(file)
    }
    fn seal(&self, attachment: &Attachment, source: &Path) -> Result<String> {
        let mut bytes = b"intercat.attachment.v1\0".to_vec();
        bytes.extend(serde_json::to_vec(&(attachment, source))?);
        Ok(hex(self.key.tag(&bytes)))
    }
    pub(crate) fn import(&self, id: Uuid, source: &Path) -> Result<Stored> {
        if id.get_version_num() != 7 {
            return Err("An attachment needs a new operation ID.".into());
        }
        self.directory(true)?;
        let source = std::path::absolute(source)?;
        let destination = self.root.join(id.to_string());
        if fs::symlink_metadata(&destination).is_ok() {
            let (stored, _, original) = self.contents(id)?;
            if original.is_some() || stored.source != source {
                return Err("Attachment operation ID reused for another file.".into());
            }
            return Ok(stored);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&source)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err("Choose a regular file as a reference.".into());
        }
        if before.len() > FILE_LIMIT as u64 {
            return Err("This file exceeds the 25 MiB copy limit.".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > FILE_LIMIT
            || !unchanged(&before, &file.metadata()?)
            || !unchanged(&before, &fs::symlink_metadata(&source)?)
        {
            return Err("The reference file changed while being copied.".into());
        }
        if bytes.len() > IMAGE_LIMIT && image::guess_format(&bytes).is_ok() {
            return Err("This image exceeds the 10 MiB reference limit.".into());
        }
        let name = source
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && s.len() <= 255)
            .ok_or("Invalid attachment name.")?
            .to_owned();
        let attachment = Attachment {
            id,
            name,
            byte_len: bytes.len() as u64,
            sha256: hex(Sha256::digest(&bytes)),
        };
        let stored = Stored {
            seal: self.seal(&attachment, &source)?,
            attachment,
            source,
        };
        let header = serde_json::to_vec(&stored)?;
        self.persist(stored, MAGIC, header, None, &bytes)
    }
    pub(crate) fn read_text_source(source: &Path) -> Result<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() > TEXT_LIMIT as u64 {
            return Err("Pasted text must be a regular UTF-8 file of at most 2 MiB.".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(TEXT_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > TEXT_LIMIT
            || !unchanged(&before, &file.metadata()?)
            || !unchanged(&before, &fs::symlink_metadata(source)?)
        {
            return Err("The pasted text changed while being saved.".into());
        }
        Ok(String::from_utf8(bytes)?)
    }
    fn text_seal(&self, header: &TextHeader) -> Result<String> {
        let mut bytes = b"intercat.pasted-text.v1\0".to_vec();
        bytes.extend(serde_json::to_vec(&(
            &header.stored.attachment,
            &header.stored.source,
            header.original_length,
            &header.original_sha256,
        ))?);
        Ok(hex(self.key.tag(&bytes)))
    }
    pub(crate) fn import_text(
        &self,
        id: Uuid,
        source: &Path,
        original: &str,
        redacted: &str,
    ) -> Result<Stored> {
        if id.get_version_num() != 7 || original.len() > TEXT_LIMIT || redacted.len() > FILE_LIMIT {
            return Err("Invalid pasted text attachment.".into());
        }
        self.directory(true)?;
        let mut header = TextHeader {
            stored: Stored {
                attachment: Attachment {
                    id,
                    name: "Pasted reference.txt".into(),
                    byte_len: redacted.len() as u64,
                    sha256: hex(Sha256::digest(redacted.as_bytes())),
                },
                source: std::path::absolute(source)?,
                seal: String::new(),
            },
            original_length: original.len(),
            original_sha256: hex(Sha256::digest(original.as_bytes())),
        };
        header.stored.seal = self.text_seal(&header)?;
        let encoded = serde_json::to_vec(&header)?;
        self.persist(
            header.stored,
            TEXT_MAGIC,
            encoded,
            Some(original.as_bytes()),
            redacted.as_bytes(),
        )
    }
    pub(crate) fn screened_copy(&self, original: &Stored, text: &str) -> Result<Stored> {
        if text.len() > FILE_LIMIT {
            return Err("The screened reference exceeds the file limit.".into());
        }
        let digest = hex(Sha256::digest(text.as_bytes()));
        let attachment = Attachment {
            id: Uuid::new_v5(&original.attachment.id, digest.as_bytes()),
            name: original.attachment.name.clone(),
            byte_len: text.len() as u64,
            sha256: digest,
        };
        let stored = Stored {
            seal: self.seal(&attachment, &original.source)?,
            attachment,
            source: original.source.clone(),
        };
        self.persist(
            stored.clone(),
            MAGIC,
            serde_json::to_vec(&stored)?,
            None,
            text.as_bytes(),
        )
    }
    fn persist(
        &self,
        stored: Stored,
        magic: &[u8; 8],
        header: Vec<u8>,
        original: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<Stored> {
        if header.len() > HEADER_LIMIT {
            return Err("Attachment metadata exceeds its limit.".into());
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(magic)?;
        temporary.write_all(&(header.len() as u32).to_be_bytes())?;
        temporary.write_all(&header)?;
        if let Some(original) = original {
            temporary.write_all(original)?;
        }
        temporary.write_all(bytes)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o400))?;
        sync(temporary.as_file())?;
        match temporary.persist_noclobber(self.root.join(stored.attachment.id.to_string())) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let (current, _) = self.load(stored.attachment.id)?;
                if current != stored {
                    return Err("Concurrent attachment imports disagree.".into());
                }
            }
            Err(e) => return Err(e.error.into()),
        }
        sync(&self.directory(false)?)?;
        Ok(stored)
    }
    pub(crate) fn load(&self, id: Uuid) -> Result<(Stored, Vec<u8>)> {
        let (stored, bytes, _) = self.contents(id)?;
        Ok((stored, bytes))
    }
    pub(crate) fn text(&self, id: Uuid) -> Result<Option<(Stored, String, String)>> {
        let (stored, bytes, original) = self.contents(id)?;
        original
            .map(|original| {
                Ok((
                    stored,
                    String::from_utf8(original)?,
                    String::from_utf8(bytes)?,
                ))
            })
            .transpose()
    }
    fn contents(&self, id: Uuid) -> Result<(Stored, Vec<u8>, Option<Vec<u8>>)> {
        let directory = self.directory(false)?;
        let name = std::ffi::CString::new(id.to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let before = file.metadata()?;
        if !before.is_file()
            || before.uid() != unsafe { libc::geteuid() }
            || before.nlink() != 1
            || before.mode() & 0o377 != 0
            || before.len() > (FILE_LIMIT + TEXT_LIMIT + HEADER_LIMIT + 12) as u64
        {
            return Err("Invalid private attachment original.".into());
        }
        let mut prefix = [0; 12];
        file.read_exact(&mut prefix)?;
        let text = &prefix[..8] == TEXT_MAGIC;
        if !text && &prefix[..8] != MAGIC {
            return Err("Invalid attachment original format.".into());
        }
        let length = u32::from_be_bytes(prefix[8..].try_into()?) as usize;
        if length > HEADER_LIMIT {
            return Err("Invalid attachment header length.".into());
        }
        let mut header = vec![0; length];
        file.read_exact(&mut header)?;
        let (stored, seal, original) = if text {
            let header: TextHeader = serde_json::from_slice(&header)?;
            if header.original_length > TEXT_LIMIT {
                return Err("The pasted original exceeds 2 MiB.".into());
            }
            let seal = self.text_seal(&header)?;
            if header.stored.attachment.id != id
                || !bool::from(header.stored.seal.as_bytes().ct_eq(seal.as_bytes()))
            {
                return Err("Pasted text ownership verification failed.".into());
            }
            let mut original = vec![0; header.original_length];
            file.read_exact(&mut original)?;
            if hex(Sha256::digest(&original)) != header.original_sha256
                || std::str::from_utf8(&original).is_err()
            {
                return Err("The pasted original changed.".into());
            }
            (header.stored, seal, Some(original))
        } else {
            let stored: Stored = serde_json::from_slice(&header)?;
            let seal = self.seal(&stored.attachment, &stored.source)?;
            (stored, seal, None)
        };
        if stored.attachment.id != id || !bool::from(stored.seal.as_bytes().ct_eq(seal.as_bytes()))
        {
            return Err("Attachment ownership verification failed.".into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > FILE_LIMIT
            || stored.attachment.byte_len != bytes.len() as u64
            || stored.attachment.sha256 != hex(Sha256::digest(&bytes))
            || !unchanged(&before, &file.metadata()?)
        {
            return Err("The original attachment changed.".into());
        }
        if bytes.len() > IMAGE_LIMIT && image::guess_format(&bytes).is_ok() {
            return Err("This image exceeds the 10 MiB reference limit.".into());
        }
        if text && std::str::from_utf8(&bytes).is_err() {
            return Err("The redacted text is not UTF-8.".into());
        }
        Ok((stored, bytes, original))
    }
    pub(crate) fn materialize(
        &self,
        workspace: &Workspace,
        attachments: &[Stored],
        screened: &std::collections::BTreeMap<Uuid, Stored>,
        input_ids: &[Uuid],
    ) -> Result<Vec<Reference>> {
        if attachments.is_empty() {
            return Ok(vec![]);
        }
        let total = attachments
            .iter()
            .filter(|original| input_ids.contains(&original.attachment.id))
            .try_fold(0u64, |total, original| {
                total
                    .checked_add(
                        screened
                            .get(&original.attachment.id)
                            .unwrap_or(original)
                            .attachment
                            .byte_len,
                    )
                    .ok_or("The reference sizes overflowed.")
            })?;
        if total > INPUT_LIMIT {
            return Err("The screened references exceed 100 MiB in one request.".into());
        }
        crate::attachments::ensure(
            &workspace.path,
            &workspace.base,
            &workspace.attachment_roots,
        )?;
        let root = workspace
            .attachment_roots
            .first()
            .ok_or("The workspace has no owned attachment directory.")?;
        let mut folder = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&workspace.path)?;
        for part in root.split('/') {
            folder = open_directory(&folder, &std::ffi::CString::new(part)?)?;
        }
        let before = folder.metadata()?;
        let mut references = Vec::new();
        for expected in attachments {
            let (original, _) = self.load(expected.attachment.id)?;
            if &original != expected {
                return Err("The recorded attachment differs from its original.".into());
            }
            let copy = screened.get(&expected.attachment.id).unwrap_or(expected);
            let (stored, bytes) = self.load(copy.attachment.id)?;
            if &stored != copy {
                return Err("The screened attachment differs from its saved copy.".into());
            }
            let stored = original;
            let name = std::ffi::CString::new(copy_name(&stored.attachment))?;
            let temporary = std::ffi::CString::new(format!(".copy-{}", Uuid::now_v7()))?;
            let fd = unsafe {
                libc::openat(
                    folder.as_raw_fd(),
                    temporary.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let result = (|| -> Result<()> {
                file.write_all(&bytes)?;
                sync(&file)?;
                if unsafe {
                    libc::renameat(
                        folder.as_raw_fd(),
                        temporary.as_ptr(),
                        folder.as_raw_fd(),
                        name.as_ptr(),
                    )
                } < 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                sync(&folder)
            })();
            if result.is_err() {
                unsafe { libc::unlinkat(folder.as_raw_fd(), temporary.as_ptr(), 0) };
            }
            result?;
            references.push(Reference {
                id: stored.attachment.id,
                name: stored.attachment.name,
                path: workspace.path.join(root).join(name.to_str()?),
            });
        }
        let after = fs::symlink_metadata(workspace.path.join(root))?;
        if !after.is_dir() || before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("The attachment copy directory changed.".into());
        }
        Ok(references)
    }
}
