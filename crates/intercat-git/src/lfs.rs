//! Current Git LFS v1 pointers. Object bytes remain in their own storage.
//! Format reference: git-lfs v3.8.1, lfs/pointer.go and docs/spec.md.
use crate::{ObjectId, Repository, Result};

pub const POINTER_LIMIT: u64 = 1024;
const VERSION: &str = "https://git-lfs.github.com/spec/v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    oid: String,
    size: u64,
}
impl Pointer {
    pub fn oid(&self) -> &str {
        &self.oid
    }
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() as u64 >= POINTER_LIMIT {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?.trim();
        let mut values = [None; 3];
        let mut field = 0;
        let mut priorities = [false; 10];
        for line in text.lines().filter(|line| !line.is_empty()) {
            if field == values.len() {
                return None;
            }
            let (key, value) = line.split_once(' ')?;
            if key == ["version", "oid", "size"][field] {
                values[field] = Some(value);
                field += 1;
                continue;
            }
            let (priority, name) = key.strip_prefix("ext-")?.split_once('-')?;
            if priority.len() != 1
                || !name
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return None;
            }
            let priority = usize::from(priority.as_bytes()[0].checked_sub(b'0')?);
            if priority >= priorities.len()
                || priorities[priority]
                || !valid_oid(value.strip_prefix("sha256:")?)
            {
                return None;
            }
            priorities[priority] = true;
        }
        if values[0]? != VERSION {
            return None;
        }
        let oid = values[1]?.strip_prefix("sha256:")?;
        if !valid_oid(oid) {
            return None;
        }
        let size = values[2]?.parse::<i64>().ok()?;
        if size < 0 {
            return None;
        }
        Some(Self {
            oid: oid.into(),
            size: size as u64,
        })
    }
}
fn valid_oid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub pointer: Pointer,
}

impl Repository {
    /// Read only small regular blobs; large payloads are never loaded to look
    /// for pointer syntax. A symlink's target is a path, not an LFS pointer.
    pub fn lfs_entries(&self, tree: ObjectId) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for (path, file) in self.tree_files(tree)? {
            if !matches!(file.mode, 0o100644 | 0o100755) || self.saved_size(&file)? >= POINTER_LIMIT
            {
                continue;
            }
            if let Some(pointer) = Pointer::parse(&self.saved_bytes(&file)?) {
                entries.push(Entry { path, pointer });
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_metadata_keeps_only_current_version_and_valid_hash_size() {
        let oid = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let data = format!("version {VERSION}\noid sha256:{oid}\nsize 0\n");
        let pointer = Pointer::parse(data.as_bytes()).unwrap();
        assert_eq!(pointer.oid(), oid);
        assert_eq!(pointer.size(), 0);
        assert!(Pointer::parse(data.replace('\n', "\r\n").as_bytes()).is_some());
        for malformed in [
            data.replace("size 0", "size -1"),
            data.replace("size 0", "size 9223372036854775808"),
            data.replace(oid, &"F".repeat(64)),
            data.replace(
                "https://git-lfs.github.com/spec/v1",
                "http://git-media.io/v/2",
            ),
            format!("{data}unexpected line\n"),
        ] {
            assert!(Pointer::parse(malformed.as_bytes()).is_none());
        }
        assert!(Pointer::parse(b"").is_none());
        assert!(Pointer::parse(b"Ordinary generated file.\n").is_none());
    }

    #[test]
    fn saved_tree_selects_small_regular_pointers_without_interpreting_other_blobs() -> Result<()> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let data = format!("version {VERSION}\noid sha256:{}\nsize 7\n", "a".repeat(64));
        let pointer = repo.write_blob(data.as_bytes())?;
        let mut large = data.as_bytes().to_vec();
        large.resize(2048, b' ');
        let large = repo.write_blob(&large)?;
        let text = repo.write_blob(b"Generated ordinary text.\n")?;
        let tree = repo.write_tree(&[
            ("assets/payload", pointer),
            ("large", large),
            ("ordinary", text),
        ])?;
        let mut edit = repo.0.edit_tree(tree)?;
        edit.upsert("link", gix::objs::tree::EntryKind::Link, pointer)?;
        let entries = repo.lfs_entries(edit.write()?.detach())?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "assets/payload");
        assert_eq!(entries[0].pointer.size(), 7);
        Ok(())
    }
}
