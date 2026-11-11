//! Read the selected GitHub CLI login offline. Values stay in memory.
use crate::{Result, command, github_pull::Token, project::Folder};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;
pub struct Credentials {
    _private: tempfile::TempDir,
    binary: PathBuf,
    origin: PathBuf,
    home: PathBuf,
    configuration: PathBuf,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or("GitHub authentication needs UTF-8 paths.")?,
    )?)
}
impl Credentials {
    pub fn system() -> Result<Self> {
        let path = std::env::var_os("PATH")
            .ok_or("GitHub CLI is unavailable. Install gh and sign in before opening PR.")?;
        let binary = std::env::split_paths(&path)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("gh"))
            .find(|path| path.is_file())
            .ok_or("GitHub CLI is unavailable. Install gh and sign in before opening PR.")?;
        let home = PathBuf::from(
            std::env::var_os("HOME").ok_or("GitHub authentication needs a home directory.")?,
        );
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let configuration = std::env::var_os("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg.join("gh"));
        Self::open(&binary, &home, &configuration)
    }
    pub fn open(binary: &Path, home: &Path, configuration: &Path) -> Result<Self> {
        let binary = binary.canonicalize()?;
        let metadata = fs::metadata(&binary)?;
        if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 {
            return Err("GitHub CLI must be a regular executable no larger than 128 MiB.".into());
        }
        let private = tempfile::Builder::new()
            .prefix("intercat-github-auth-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let staged = private.path().canonicalize()?.join("gh");
        fs::copy(&binary, &staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o500))?;
        Ok(Self {
            _private: private,
            binary: staged,
            origin: binary,
            home: crate::physical_path::resolve(home)?,
            configuration: crate::physical_path::resolve(configuration)?,
        })
    }
    fn check_folder(&self, folder: &Folder) -> Result<()> {
        if self.origin.starts_with(&folder.root) || self.origin.starts_with(&folder.common_dir) {
            return Err("GitHub CLI must be installed outside the source repository.".into());
        }
        if self.configuration.starts_with(&folder.root)
            || self.configuration.starts_with(&folder.common_dir)
            || self.home.starts_with(&folder.root)
        {
            return Err("GitHub authentication must be outside the project repository.".into());
        }
        Ok(())
    }
    pub async fn token(&self, folders: &[Folder], cancel: watch::Receiver<bool>) -> Result<Token> {
        if folders.is_empty() || folders.len() > 8 {
            return Err("GitHub authentication needs one to eight reviewed folders.".into());
        }
        for folder in folders {
            self.check_folder(folder)?;
        }
        let root = self.binary.parent().unwrap();
        let mut profile = format!(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath {}) (subpath {}) (subpath {}))(allow file-write* (literal \"/dev/null\"))(allow process-exec (literal {}) (literal \"/usr/bin/security\") (literal \"/bin/sh\") (literal \"/bin/bash\"))",
            quote(root)?,
            quote(&self.home)?,
            quote(&self.configuration)?,
            quote(&self.binary)?,
        );
        for folder in folders {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {}) (subpath {}))",
                quote(&folder.root)?,
                quote(&folder.common_dir)?
            ));
        }
        // Parent directory reads are literal, never broad access to sibling projects.
        for ancestor in root.ancestors() {
            profile.push_str(&format!(
                "(allow file-read* (literal {}))",
                quote(ancestor)?
            ));
        }
        let mut child = command::gated_command("/usr/bin/sandbox-exec", None);
        child
            .args(["-p", &profile])
            .arg(&self.binary)
            .args(["auth", "token", "--hostname", "github.com", "--no-refresh"])
            .current_dir(root)
            .env("HOME", &self.home)
            .env("GH_CONFIG_DIR", &self.configuration)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GH_PROMPT_DISABLED", "1");
        let output = command::run(&mut child, cancel, Duration::from_secs(15)).await?;
        if !output.success() || output.truncated {
            return Err("Sign in to GitHub CLI before opening PR. Authentication was not sent to a server by this step.".into());
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "GitHub authentication returned invalid data.")?;
        Token::new(text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fixed_auth_reader_is_offline_and_cannot_read_or_change_project_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let home = root.join("home");
        let source = root.join("source");
        fs::create_dir(&home)?;
        fs::create_dir(&source)?;
        fs::create_dir(source.join(".git"))?;
        fs::write(source.join("private-dummy"), "own-dummy-only\n")?;
        let script = root.join("own dummy gh");
        let protected = source.join("private-dummy");
        let path = shlex::try_join([protected.to_str().unwrap()]).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$*\" = 'auth token --hostname github.com --no-refresh' ] || exit 2\nif IFS= read -r value < {path}; then exit 3; fi\nif printf altered > {path}; then exit 4; fi\nprintf dummy-token-local-only\n"
            ),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let credentials = Credentials::open(&script, &home, &home.join(".config/gh"))?;
        let folder = Folder {
            id: uuid::Uuid::now_v7().to_string(),
            name: "Own dummy".into(),
            path: source.clone(),
            root: source.clone(),
            common_dir: source.join(".git"),
            base: "a".repeat(40),
            base_branch: Some("refs/heads/main".into()),
        };
        let _token = credentials
            .token(&[folder], watch::channel(false).1)
            .await?;
        assert_eq!(fs::read_to_string(protected)?, "own-dummy-only\n");
        Ok(())
    }
    #[tokio::test]
    async fn a_project_owned_gh_cannot_acquire_authentication_permissions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().canonicalize()?;
        let source = path.join("source");
        let home = path.join("home");
        fs::create_dir(&source)?;
        fs::create_dir(&home)?;
        fs::create_dir(source.join(".git"))?;
        let script = source.join("gh");
        fs::write(&script, "#!/bin/sh\nprintf dummy-token-local-only\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let credentials = Credentials::open(&script, &home, &home.join("gh-config"))?;
        let folder = Folder {
            id: uuid::Uuid::now_v7().to_string(),
            name: "Own source".into(),
            path: source.clone(),
            root: source.clone(),
            common_dir: source.join(".git"),
            base: "a".repeat(40),
            base_branch: Some("refs/heads/main".into()),
        };
        let result = credentials.token(&[folder], watch::channel(false).1).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("outside the source repository")
        );
        Ok(())
    }
}
