//! Physical names for configured storage and denied filesystem boundaries.
use crate::Result;
use std::path::{Component, Path};

/// Resolve existing ancestors and retain an absent tail without creating it.
/// Callers freeze this physical name before constructing filesystem boundaries.
pub fn resolve(path: &Path) -> Result<std::path::PathBuf> {
    if !path.is_absolute() {
        return Err("A physical path must be absolute.".into());
    }
    let mut normalized = std::path::PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                normalized.push(name);
                match normalized.canonicalize() {
                    Ok(physical) => normalized = physical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if std::fs::symlink_metadata(&normalized).is_ok() {
                            return Err("A physical path contains a broken symlink.".into());
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => return Err("Invalid physical path.".into()),
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_aliases_and_absent_tails_without_creating_or_accepting_broken_links() -> Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let real = root.join("real");
        std::fs::create_dir(&real)?;
        std::os::unix::fs::symlink(&real, root.join("alias"))?;
        assert_eq!(
            resolve(&root.join("alias/lfs/objects"))?,
            real.join("lfs/objects")
        );
        assert_eq!(
            resolve(&root.join("alias/missing/../lfs"))?,
            real.join("lfs")
        );
        assert!(!real.join("lfs").exists());
        std::os::unix::fs::symlink(root.join("absent"), root.join("broken"))?;
        assert!(resolve(&root.join("broken/lfs")).is_err());
        assert!(resolve(Path::new("relative/lfs")).is_err());
        Ok(())
    }
}
