//! Large privacy previews use an owner-only local file, never oversized RPC frames.
use crate::{
    Result,
    privacy::{Part, View},
};
use base64::Engine;
use std::path::Path;
pub(crate) fn read(root: &Path, id: &str) -> Result<Vec<String>> {
    if id.len() != 11
        || !id.starts_with("view-")
        || !id[5..].bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("Invalid privacy preview identifier.".into());
    }
    let bytes = crate::private_input::read(root, &[id, "input.json"], 16 * 1024 * 1024)?;
    let texts: Vec<String> =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid privacy preview file.")?;
    if texts.len() > 64
        || texts.iter().any(|text| text.len() > 2 * 1024 * 1024)
        || texts.iter().map(String::len).sum::<usize>() > 2 * 1024 * 1024
    {
        return Err("Privacy preview input is too large.".into());
    }
    Ok(texts)
}

pub(crate) fn wire(views: Vec<View>) -> serde_json::Value {
    serde_json::Value::Array(views.into_iter().map(|view| {
        serde_json::json!({"parts":view.parts.into_iter().map(|part| match part {
            Part::Text{text} => serde_json::json!({"kind":"text","textBase64":base64::prelude::BASE64_STANDARD.encode(text)}),
            Part::Redaction{id,category} => serde_json::json!({"kind":"redaction","id":id,"category":category}),
        }).collect::<Vec<_>>()})
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn private_files_are_bounded_and_reject_links_and_path_escape() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let folder = root.path().join("view-123456");
        fs::create_dir(&folder)?;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
        let file = folder.join("input.json");
        fs::write(
            &file,
            serde_json::to_vec(&vec!["x".repeat(2 * 1024 * 1024)])?,
        )?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        assert_eq!(read(root.path(), "view-123456")?[0].len(), 2 * 1024 * 1024);
        assert!(read(root.path(), "../12345678").is_err());
        fs::write(
            &file,
            serde_json::to_vec(&vec!["x".repeat(2 * 1024 * 1024 + 1)])?,
        )?;
        assert!(read(root.path(), "view-123456").is_err());
        fs::remove_file(&file)?;
        let outside = root.path().join("outside");
        fs::write(&outside, b"[\"dummy\"]")?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
        symlink(&outside, &file)?;
        assert!(read(root.path(), "view-123456").is_err());
        fs::remove_file(&file)?;
        fs::hard_link(&outside, &file)?;
        assert!(read(root.path(), "view-123456").is_err());
        Ok(())
    }
}
