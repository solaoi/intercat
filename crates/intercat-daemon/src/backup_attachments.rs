//! Read-only comparison of the restored registry, private originals and owned copies.
use crate::{
    Result,
    attachment_store::{Store, Stored, copy_name},
    backup_files::Entry,
};
use intercat_git::AttachmentPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};
use uuid::Uuid;

const LIMIT: usize = 50_000;
const BYTE_LIMIT: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Copy {
    pub path: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub state: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Review {
    pub originals: usize,
    pub verified_originals: usize,
    pub missing_originals: Vec<Uuid>,
    pub invalid_originals: Vec<Uuid>,
    pub unrecorded_originals: Vec<String>,
    pub copies: Vec<Copy>,
    pub complete: bool,
}
type Stamp = (u64, u64, u32, u64, u64, i64, i64, i64, i64);
fn stamp(m: &fs::Metadata) -> Stamp {
    (
        m.dev(),
        m.ino(),
        m.mode(),
        m.nlink(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
fn inventory(store: &Store) -> Result<BTreeMap<String, Stamp>> {
    let mut entries = BTreeMap::new();
    let directory = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&store.root)
    {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    let root = directory.metadata()?;
    if root.uid() != unsafe { libc::geteuid() } || root.mode() & 0o077 != 0 {
        return Err("The original attachment directory is not private.".into());
    }
    entries.insert(String::new(), stamp(&root));
    for entry in fs::read_dir(&store.root)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "Invalid private attachment filename.")?;
        entries.insert(name, stamp(&fs::symlink_metadata(entry.path())?));
        if entries.len() > LIMIT {
            return Err("Too many private attachment originals to review.".into());
        }
    }
    if stamp(&directory.metadata()?) != stamp(&root)
        || stamp(&fs::symlink_metadata(&store.root)?) != stamp(&root)
    {
        return Err("The original attachment directory changed during review.".into());
    }
    Ok(entries)
}
pub(crate) struct Originals {
    report: Review,
    records: BTreeMap<Uuid, Stored>,
    digests: BTreeMap<Uuid, Vec<u8>>,
    pub material: Vec<u8>,
}
pub(crate) fn originals(store: &Store, records: &[Stored]) -> Result<Originals> {
    if records.len() > LIMIT {
        return Err("Too many recorded originals to review.".into());
    }
    let before = inventory(store)?;
    let known = records
        .iter()
        .map(|s| s.attachment.id.to_string())
        .collect::<BTreeSet<_>>();
    let mut report = Review {
        originals: records.len(),
        complete: true,
        ..Default::default()
    };
    report.unrecorded_originals = before
        .keys()
        .filter(|name| !name.is_empty() && !known.contains(*name))
        .cloned()
        .collect();
    let mut total = 0u64;
    let mut digests = BTreeMap::new();
    for expected in records {
        let id = expected.attachment.id;
        if !before.contains_key(&id.to_string()) {
            report.missing_originals.push(id);
            continue;
        }
        total = total
            .checked_add(before[&id.to_string()].4)
            .ok_or("Attachment review size overflow.")?;
        if total > BYTE_LIMIT {
            return Err("Private originals exceed the review byte limit.".into());
        }
        match store.load(id) {
            Ok((stored, bytes)) if &stored == expected => {
                digests.insert(id, Sha256::digest(bytes).to_vec());
            }
            _ => report.invalid_originals.push(id),
        }
    }
    if before != inventory(store)? {
        return Err("Private originals changed during review.".into());
    }
    report.verified_originals = digests.len();
    report.complete = report.missing_originals.is_empty()
        && report.invalid_originals.is_empty()
        && report.unrecorded_originals.is_empty();
    Ok(Originals {
        material: serde_json::to_vec(&(records, before))?,
        records: records
            .iter()
            .map(|s| (s.attachment.id, s.clone()))
            .collect(),
        digests,
        report,
    })
}

pub(crate) fn compare(
    originals: Originals,
    entries: &BTreeMap<String, Entry>,
    paths: &AttachmentPaths,
    roots: &[String],
    required: &[Uuid],
) -> (Review, Vec<u8>) {
    let mut report = originals.report;
    let expected = required
        .iter()
        .filter_map(|id| {
            originals
                .records
                .get(id)
                .map(|s| (copy_name(&s.attachment), s))
        })
        .collect::<BTreeMap<_, _>>();
    let first_root = roots.first();
    let mut found = BTreeSet::new();
    for root in roots {
        if !entries.get(root).is_some_and(Entry::owned_directory) {
            report.copies.push(Copy {
                path: Some(root.clone()),
                attachment_id: None,
                state: "unsafe-directory".into(),
            });
        }
    }
    for (path, entry) in entries {
        if !paths.potential_copy(path) || roots.contains(path) {
            continue;
        }
        if !paths.excludes(path) {
            report.copies.push(Copy {
                path: Some(path.clone()),
                attachment_id: None,
                state: "unowned".into(),
            });
            continue;
        }
        let Some((parent, name)) = path.rsplit_once('/') else {
            continue;
        };
        if roots.iter().any(|root| parent == root) && name == ".gitignore" {
            if !entry.matches(&Sha256::digest(b"*\n")) {
                report.copies.push(Copy {
                    path: Some(path.clone()),
                    attachment_id: None,
                    state: "changed-control-file".into(),
                });
            }
            continue;
        }
        let Some(stored) = expected
            .get(name)
            .filter(|_| Some(parent) == first_root.map(String::as_str))
        else {
            report.copies.push(Copy {
                path: Some(path.clone()),
                attachment_id: None,
                state: "unrecorded".into(),
            });
            continue;
        };
        let id = stored.attachment.id;
        found.insert(id);
        report.copies.push(Copy {
            path: Some(path.clone()),
            attachment_id: Some(id),
            state: match originals.digests.get(&id) {
                Some(digest) if entry.matches(digest) => "matched",
                Some(_) => "changed",
                None => "original-unverified",
            }
            .into(),
        });
    }
    for id in required.iter().filter(|id| !found.contains(id)) {
        report.copies.push(Copy {
            path: first_root.and_then(|root| {
                originals
                    .records
                    .get(id)
                    .map(|s| format!("{root}/{}", copy_name(&s.attachment)))
            }),
            attachment_id: Some(*id),
            state: "missing".into(),
        });
    }
    report
        .copies
        .sort_by(|a, b| (&a.path, &a.attachment_id).cmp(&(&b.path, &b.attachment_id)));
    report.complete &= report.copies.iter().all(|copy| copy.state == "matched");
    (report, originals.material)
}

pub(crate) struct Source {
    pub store: Store,
    pub records: Vec<Stored>,
    pub roots: Vec<String>,
    pub required: Vec<Uuid>,
}
