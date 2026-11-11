//! Authentication settings stay in memory. Repository code is never accepted
//! as a credential helper in the network-enabled transport process.
use crate::{Result, git_tool::GitTool, project::Folder, push_inspect::Inspector};
use std::{
    ffi::OsStr,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};
use tokio::sync::watch;

pub struct Authentication {
    pub(crate) folder_id: String,
    pub(crate) destination: crate::push_target::Destination,
    pub(crate) system: bool,
    pub(crate) home: PathBuf,
    pub(crate) xdg: PathBuf,
    pub(crate) values: Vec<(String, String)>,
    pub(crate) helpers: Vec<PathBuf>,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) agent: Option<PathBuf>,
}
pub(crate) fn agent_socket(folder: &Folder, agent: &Path) -> Result<PathBuf> {
    let agent = agent.canonicalize()?;
    if !std::fs::metadata(&agent)?.file_type().is_socket()
        || agent.starts_with(&folder.root)
        || agent.starts_with(&folder.common_dir)
    {
        return Err("The SSH agent must be a socket outside the project.".into());
    }
    Ok(agent)
}
fn find(program: &str, path: &OsStr, git: &GitTool, builtin: bool) -> Result<PathBuf> {
    let input = Path::new(program);
    let candidates = if input.is_absolute() {
        vec![input.to_path_buf()]
    } else if input.components().count() != 1 {
        return Err("Git authentication needs an absolute helper path or a command name.".into());
    } else {
        let name = if builtin {
            format!("git-credential-{program}")
        } else {
            program.into()
        };
        std::iter::once(git.exec_dir().to_path_buf())
            .chain(std::env::split_paths(path))
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(&name))
            .collect()
    };
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate.canonicalize()?);
        }
    }
    Err("The configured Git credential helper is unavailable.".into())
}
fn helper(
    value: &str,
    scope: &str,
    folder: &Folder,
    git: &GitTool,
    path: &OsStr,
) -> Result<Option<(String, PathBuf)>> {
    if value.is_empty() {
        return Ok(None);
    }
    let (script, words) = match value.strip_prefix('!') {
        Some(command) => (true, command),
        None => (false, value),
    };
    if words
        .chars()
        .any(|c| c.is_control() || "$`;|&><{}()".contains(c))
    {
        return Err("This credential helper needs shell evaluation. Choose a helper command with literal arguments before Push.".into());
    }
    let words = shlex::split(words).ok_or("The credential helper has invalid quoting.")?;
    let program = words
        .first()
        .ok_or("The credential helper has no command.")?;
    if !["global", "system"].contains(&scope)
        && (script || words.len() != 1 || program != "osxkeychain")
    {
        return Err("Move this credential helper to your user Git settings before Push. Repository helpers cannot run with network access.".into());
    }
    let named = !script && !Path::new(program).is_absolute();
    let resolved = find(program, path, git, !script)?;
    if resolved.starts_with(&folder.root) || resolved.starts_with(&folder.common_dir) {
        return Err(
            "A credential helper inside the project cannot run with network access.".into(),
        );
    }
    let mut command_words = if named {
        vec![
            git.binary()
                .to_str()
                .ok_or("Git authentication needs a UTF-8 tool path.")?
                .to_owned(),
            format!("credential-{program}"),
        ]
    } else {
        vec![
            resolved
                .to_str()
                .ok_or("Git authentication needs a UTF-8 helper path.")?
                .to_owned(),
        ]
    };
    command_words.extend(words.into_iter().skip(1));
    let command = shlex::try_join(command_words.iter().map(String::as_str))
        .map_err(|_| "Invalid credential helper arguments.")?;
    Ok(Some((format!("!exec {command}"), resolved)))
}
impl Authentication {
    pub async fn capture(
        inspector: &Inspector,
        folder: &Folder,
        git: &GitTool,
        destination: &str,
        path: &OsStr,
        agent: Option<&Path>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let (home, xdg) = inspector.user_paths();
        let mut auth = Self {
            folder_id: folder.id.clone(),
            destination: crate::push_target::Destination::capture(destination)?,
            system: inspector.system(),
            home: home.into(),
            xdg: xdg.into(),
            values: Vec::new(),
            helpers: Vec::new(),
            files: Vec::new(),
            agent: None,
        };
        let mut bytes = 0usize;
        let settings = if matches!(
            intercat_git::RemoteAddress::parse(destination)?.transport,
            intercat_git::Transport::Http | intercat_git::Transport::Https
        ) {
            inspector.authentication(folder, cancel).await?
        } else {
            Vec::new()
        };
        for (scope, key, mut value) in settings {
            let field = key.rsplit('.').next().unwrap_or("");
            if key.starts_with("credential.") {
                if !["helper", "username", "usehttppath"].contains(&field) {
                    continue;
                }
                if field == "helper"
                    && let Some((prepared, program)) = helper(&value, &scope, folder, git, path)?
                {
                    value = prepared;
                    auth.helpers.push(program);
                }
            } else if key.starts_with("http.") {
                if field == "proxy" && !value.is_empty() {
                    return Err("This Push uses an HTTP proxy. Its connection must be reviewed before sending.".into());
                }
                if field == "proxy" {
                    continue;
                }
                if ![
                    "sslverify",
                    "sslcainfo",
                    "sslcapath",
                    "sslcert",
                    "sslkey",
                    "sslcertpasswordprotected",
                    "extraheader",
                    "cookiefile",
                    "version",
                    "emptyauth",
                    "proxy",
                ]
                .contains(&field)
                {
                    continue;
                }
                if ["sslcainfo", "sslcapath", "sslcert", "sslkey", "cookiefile"].contains(&field)
                    && !value.is_empty()
                {
                    let expanded = value.strip_prefix("~/").map(|tail| auth.home.join(tail));
                    let file = expanded.as_deref().unwrap_or(Path::new(&value));
                    if !file.is_absolute() {
                        return Err(
                            "Git authentication files need absolute paths before Push.".into()
                        );
                    }
                    let file = crate::physical_path::resolve(file)?;
                    if file.starts_with(&folder.root) || file.starts_with(&folder.common_dir) {
                        return Err(
                            "Move the Git authentication file outside the project before Push."
                                .into(),
                        );
                    }
                    value = file
                        .to_str()
                        .ok_or("Git authentication needs a UTF-8 file path.")?
                        .into();
                    auth.files.push(file);
                }
            } else {
                continue;
            }
            bytes = bytes.saturating_add(key.len()).saturating_add(value.len());
            if auth.values.len() >= 256 || bytes > 1024 * 1024 {
                return Err("Git authentication configuration exceeds its limit.".into());
            }
            auth.values.push((key, value));
        }
        if let Some(agent) = agent {
            auth.agent = Some(agent_socket(folder, agent)?);
        }
        auth.helpers.sort();
        auth.helpers.dedup();
        auth.files.sort();
        auth.files.dedup();
        Ok(auth)
    }
}
