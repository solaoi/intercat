//! Only the endpoint keys that git-lfs permits in the frozen `.lfsconfig`.
use crate::{ObjectId, Repository, Result};
use std::collections::BTreeMap;

impl Repository {
    pub fn push_lfs_configuration(
        &self,
        head: ObjectId,
        remote: &str,
    ) -> Result<BTreeMap<String, String>> {
        let tree = self.0.find_commit(head)?.tree()?;
        let Some(entry) = tree.find_entry(".lfsconfig") else {
            return Ok(BTreeMap::new());
        };
        if !entry.mode().is_blob() {
            return Err("The frozen .lfsconfig must be a regular Git blob.".into());
        }
        if self.0.find_header(entry.object_id())?.size() > 1024 * 1024 {
            return Err("The frozen .lfsconfig exceeds 1 MiB.".into());
        }
        let bytes = self.0.find_blob(entry.object_id())?;
        let config = gix::config::File::from_bytes_no_includes(
            &bytes.data,
            gix::config::file::Metadata::default(),
            Default::default(),
        )
        .map_err(|_| "The frozen .lfsconfig could not be parsed.")?;
        let mut values = BTreeMap::new();
        for key in [
            "lfs.url".into(),
            "lfs.pushurl".into(),
            "lfs.gitprotocol".into(),
            format!("remote.{remote}.lfsurl"),
        ] {
            if let Some(value) = config.string(key.as_str()) {
                values.insert(
                    key,
                    std::str::from_utf8(&value)
                        .map_err(|_| "The frozen .lfsconfig contains a non-UTF8 endpoint.")?
                        .into(),
                );
            }
        }
        Ok(values)
    }
}
