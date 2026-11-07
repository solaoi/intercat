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
    digests: BTreeMap<Uuid, Vec<Vec<u8>>>,
    pub material: Vec<u8>,
}
pub(crate) fn originals(
    store: &Store,
    records: &[Stored],
    screenings: &[crate::attachment_screening::Revision],
) -> Result<Originals> {
    if records.len() > LIMIT {
        return Err("Too many recorded originals to review.".into());
    }
    let before = inventory(store)?;
    let mut known = records
        .iter()
        .map(|s| s.attachment.id.to_string())
        .collect::<BTreeSet<_>>();
    known.extend(
        screenings
            .iter()
            .map(|revision| revision.copy.attachment.id.to_string()),
    );
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
                digests.insert(id, vec![Sha256::digest(bytes).to_vec()]);
            }
            _ => report.invalid_originals.push(id),
        }
    }
    let originals = records
        .iter()
        .map(|stored| (stored.attachment.id, stored))
        .collect::<BTreeMap<_, _>>();
    let mut checked = BTreeSet::new();
    for revision in screenings {
        let id = revision.original.attachment.id;
        if !digests.contains_key(&id) || originals.get(&id).copied() != Some(&revision.original) {
            report.invalid_originals.push(id);
            continue;
        }
        let copy_id = revision.copy.attachment.id;
        if copy_id == id {
            continue;
        }
        if checked.insert(copy_id) {
            total = total
                .checked_add(before.get(&copy_id.to_string()).map_or(0, |value| value.4))
                .ok_or("Attachment review size overflow.")?;
            if total > BYTE_LIMIT {
                return Err(
                    "Private originals and screened copies exceed the review byte limit.".into(),
                );
            }
        }
        match store.load(copy_id) {
            Ok((stored, bytes)) if stored == revision.copy => {
                digests
                    .get_mut(&id)
                    .unwrap()
                    .push(Sha256::digest(bytes).to_vec());
            }
            _ => report.invalid_originals.push(id),
        }
    }
    report.invalid_originals.sort_unstable();
    report.invalid_originals.dedup();
    for id in &report.invalid_originals {
        digests.remove(id);
    }
    if before != inventory(store)? {
        return Err("Private originals changed during review.".into());
    }
    report.verified_originals = digests.len();
    report.complete = report.missing_originals.is_empty()
        && report.invalid_originals.is_empty()
        && report.unrecorded_originals.is_empty();
    Ok(Originals {
        material: serde_json::to_vec(&(records, screenings, before))?,
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
                Some(digests) if digests.iter().any(|digest| entry.matches(digest)) => "matched",
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
    pub screenings: Vec<crate::attachment_screening::Revision>,
    pub roots: Vec<String>,
    pub required: Vec<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{attachment_screening::Revision, secrets::Key};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    #[test]
    fn approved_copies_are_verified_with_their_original_and_corruption_is_rejected() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store {
            root: root.path().join("attachments"),
            key: Arc::new(Key::new([7; 32])),
        };
        let original = store.import_text(
            Uuid::now_v7(),
            &root.path().join("paste.txt"),
            "Original private value",
            "Original private value",
        )?;
        let copy = store.screened_copy(&original, "Original <secret:TOKEN>")?;
        let revision = Revision {
            original: original.clone(),
            copy: copy.clone(),
        };
        let checked = originals(
            &store,
            std::slice::from_ref(&original),
            std::slice::from_ref(&revision),
        )?;
        assert!(checked.report.complete);
        assert_eq!(checked.report.verified_originals, 1);
        assert_eq!(checked.digests[&original.attachment.id].len(), 2);
        let path = store.root.join(copy.attachment.id.to_string());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        fs::write(&path, "corrupt copy")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        let checked = originals(
            &store,
            std::slice::from_ref(&original),
            std::slice::from_ref(&revision),
        )?;
        assert!(!checked.report.complete);
        assert_eq!(
            checked.report.invalid_originals,
            vec![original.attachment.id]
        );
        Ok(())
    }
}
