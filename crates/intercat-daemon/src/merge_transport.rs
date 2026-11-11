//! RPC carries an opaque input identifier and digest, never the editor body.
use crate::{Result, merge_manual::Apply};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Uploaded {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub digest: String,
    pub input: String,
}
pub fn read(root: &Path, input: &Uploaded) -> Result<Apply> {
    let prefix = format!("{}-", input.op_id);
    if input.op_id.get_version_num() != 7
        || !input.input.starts_with(&prefix)
        || input.input.len() != prefix.len() + 6
        || !input.input[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("Invalid manual editor input identifier.".into());
    }
    let bytes = crate::private_input::read(root, &[&input.input, "input.json"], 8 * 1024 * 1024)?;
    if crate::merge_editor::digest(&bytes) != input.digest {
        return Err("The manual editor input changed.".into());
    }
    let request: Apply =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid manual editor input.")?;
    if request.op_id != input.op_id || request.id != input.id || request.revision != input.revision
    {
        return Err("The manual editor input belongs to another request.".into());
    }
    if crate::merge_editor::digest(&serde_json::to_vec(&request)?) != input.digest {
        return Err("The manual editor input is not canonical.".into());
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn uploaded_edits_are_bounded_bound_to_the_request_and_never_follow_links() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let op = Uuid::now_v7();
        let request = Apply {
            op_id: op,
            id: Uuid::now_v7(),
            revision: 8,
            files: vec![crate::merge_manual::Edit::Text {
                path: "dummy.txt".into(),
                version: "dummy-version".into(),
                text: "日本語\n".repeat(60000),
            }],
        };
        let bytes = serde_json::to_vec(&request)?;
        assert!(bytes.len() > 512 * 1024);
        let input = Uploaded {
            op_id: op,
            id: request.id,
            revision: 8,
            digest: crate::merge_editor::digest(&bytes),
            input: format!("{op}-ABC123"),
        };
        let box_ = root.path().join(&input.input);
        fs::create_dir(&box_)?;
        fs::set_permissions(&box_, fs::Permissions::from_mode(0o700))?;
        let path = box_.join("input.json");
        fs::write(&path, &bytes)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        assert_eq!(serde_json::to_vec(&read(root.path(), &input)?)?, bytes);
        assert!(
            read(
                root.path(),
                &Uploaded {
                    digest: "0".repeat(64),
                    ..input.clone()
                }
            )
            .is_err()
        );
        assert!(
            read(
                root.path(),
                &Uploaded {
                    id: Uuid::now_v7(),
                    ..input.clone()
                }
            )
            .is_err()
        );
        assert!(
            read(
                root.path(),
                &Uploaded {
                    input: "../dummy".into(),
                    ..input.clone()
                }
            )
            .is_err()
        );
        fs::remove_file(&path)?;
        let outside = root.path().join("dummy-reference");
        fs::write(&outside, &bytes)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::symlink(&outside, &path)?;
        assert!(read(root.path(), &input).is_err());
        fs::remove_file(&path)?;
        fs::hard_link(&outside, &path)?;
        assert!(read(root.path(), &input).is_err());
        Ok(())
    }
}
