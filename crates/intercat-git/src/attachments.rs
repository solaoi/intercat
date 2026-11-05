//! Reserved copy directories, with tracked project content taking precedence.
use crate::Result;
use std::{collections::HashSet, fs, path::Path};

const DEFAULT: &str = ".intercat/attachments";

#[derive(Clone, Debug)]
pub struct AttachmentPaths {
    occupied: HashSet<String>,
    owned: Vec<String>,
}
fn root(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if lower == DEFAULT || lower.starts_with(".intercat/attachments/") {
        return Some(DEFAULT.into());
    }
    let first = lower.split('/').next()?;
    let number = first.strip_prefix(".intercat-attachments-")?;
    if !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && number.parse::<u32>().is_ok_and(|number| number >= 2)
    {
        Some(first.into())
    } else {
        None
    }
}
impl AttachmentPaths {
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut occupied = HashSet::new();
        for path in paths {
            if path.eq_ignore_ascii_case(".intercat") {
                occupied.insert(DEFAULT.into());
            }
            if let Some(root) = root(path) {
                occupied.insert(root);
            }
        }
        Self {
            occupied,
            owned: Vec::new(),
        }
    }
    pub fn with_roots(mut self, roots: &[String]) -> Result<Self> {
        for path in roots {
            let key = root(path)
                .filter(|key| key.len() == path.len())
                .ok_or("Invalid attachment directory.")?;
            if self.occupied.contains(&key) {
                return Err("An attachment directory overlaps tracked project files.".into());
            }
            self.owned.push(key);
        }
        self.owned.sort();
        self.owned.dedup();
        Ok(self)
    }
    /// Only directories reserved by the app are excluded. An unregistered
    /// directory with a familiar name is still user data.
    pub fn excludes(&self, path: &str) -> bool {
        root(path).is_some_and(|root| self.owned.contains(&root))
    }
    pub fn potential_copy(&self, path: &str) -> bool {
        root(path).is_some_and(|root| !self.occupied.contains(&root))
    }
    pub(crate) fn excluded_root(&self, path: &str) -> Option<String> {
        let key = root(path)?;
        self.owned
            .contains(&key)
            .then(|| path[..key.len()].to_owned())
    }
    /// Choose an empty location. Existing untracked files and symlinks count
    /// as occupied too; callers must still create the chosen directory exclusively.
    pub fn directory(&self, workspace: &Path) -> Result<String> {
        for index in 1..=10000 {
            let root = if index == 1 {
                DEFAULT.to_owned()
            } else {
                format!(".intercat-attachments-{index}")
            };
            if self.occupied.contains(&root) {
                continue;
            }
            if index == 1 {
                match fs::symlink_metadata(workspace.join(".intercat")) {
                    Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => continue,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            match fs::symlink_metadata(workspace.join(&root)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(root),
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
        }
        Err("No empty attachment directory is available.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn occupied_project_roots_are_kept_and_new_copy_namespaces_are_excluded() {
        let paths = AttachmentPaths::from_paths([
            ".intercat/attachments/code.txt",
            ".intercat-attachments-2/lib.txt",
        ])
        .with_roots(&[".intercat-attachments-3".into()])
        .unwrap();
        assert!(!paths.excludes(".intercat/attachments/new-code.txt"));
        assert!(!paths.excludes(".INTERCAT/ATTACHMENTS/code.txt"));
        assert!(!paths.excludes(".intercat-attachments-2/lib.txt"));
        assert!(paths.excludes(".intercat-attachments-3/reference.txt"));
        assert!(!paths.excludes("src/.intercat/attachments/code.txt"));
        assert!(!paths.excludes(".intercat-attachments-notes/code.txt"));
        assert!(!paths.excludes(".intercat-attachments-1/code.txt"));
        assert!(!paths.excludes(".intercat-attachments-4/user.txt"));
        assert!(
            AttachmentPaths::from_paths([".intercat/attachments/code.txt"])
                .with_roots(&[DEFAULT.into()])
                .is_err()
        );
        assert!(
            AttachmentPaths::from_paths([])
                .with_roots(&[".intercat/attachments/../src".into()])
                .is_err()
        );
        let file = AttachmentPaths::from_paths([".intercat"])
            .with_roots(&[".intercat-attachments-2".into()])
            .unwrap();
        assert!(!file.excludes(".intercat/attachments/reference.txt"));
        assert!(file.excludes(".intercat-attachments-2/reference.txt"));
    }
    #[test]
    fn directory_selection_never_reuses_existing_files_or_follows_a_parent_link() -> Result<()> {
        let root = tempfile::tempdir()?;
        let paths = AttachmentPaths::from_paths(std::iter::empty());
        assert_eq!(paths.directory(root.path())?, ".intercat/attachments");
        std::os::unix::fs::symlink(root.path().join("outside"), root.path().join(".intercat"))?;
        assert_eq!(paths.directory(root.path())?, ".intercat-attachments-2");
        fs::write(root.path().join(".intercat-attachments-2"), "user file")?;
        assert_eq!(paths.directory(root.path())?, ".intercat-attachments-3");
        fs::create_dir(root.path().join(".intercat-attachments-3"))?;
        assert_eq!(paths.directory(root.path())?, ".intercat-attachments-4");
        let paths = AttachmentPaths::from_paths([
            ".intercat/attachments/tracked.txt",
            ".intercat-attachments-4/code.txt",
        ]);
        assert_eq!(paths.directory(root.path())?, ".intercat-attachments-5");
        assert_eq!(
            fs::read_to_string(root.path().join(".intercat-attachments-2"))?,
            "user file"
        );
        Ok(())
    }
}
