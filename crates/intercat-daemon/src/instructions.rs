//! Project rules are explicitly scoped when the native host runs without project trust.
use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

const NAMES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".agents/AGENTS.md",
    ".claude/CLAUDE.md",
];
const FILE_LIMIT: u64 = 64 * 1024;
const TOTAL_LIMIT: usize = 512 * 1024;

fn read_rule(root: &File, path: &Path) -> Result<Option<(Vec<u8>, u32)>> {
    let mut parent = root.try_clone()?;
    let parts = path.components().collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("Invalid instruction path.".into());
        };
        let name = CString::new(name.as_bytes())?;
        let last = index + 1 == parts.len();
        // Pin every ancestor. O_NOFOLLOW on the leaf alone still follows a
        // symlink used as .agents, .claude or another enclosing directory.
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error.into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err("Instruction file must be a regular file.".into());
            }
            let mut bytes = Vec::new();
            file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > FILE_LIMIT {
                return Err("Instruction file exceeds 64 KiB.".into());
            }
            return Ok(Some((
                bytes,
                if metadata.mode() & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                },
            )));
        }
        parent = file;
    }
    Err("Empty instruction path.".into())
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub path: PathBuf,
    pub scope: PathBuf,
    pub sha256: String,
    pub text: String,
    #[serde(skip)]
    pub mode: u32,
}
#[derive(Debug, Serialize)]
pub struct Instructions {
    pub rules: Vec<Rule>,
    pub digest: String,
}

/// All four names are retained for a branch, including lower-priority files.
/// Only the selected file in each scope is sent as an instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Files(pub BTreeMap<String, InstructionFile>);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFile {
    pub text: String,
    pub mode: u32,
}

impl Files {
    pub fn capture(root: &Path) -> Result<Self> {
        Ok(Self(
            capture(root, false)?
                .into_iter()
                .map(|rule| -> Result<_> {
                    Ok((
                        rule.path
                            .to_str()
                            .ok_or("Instruction paths must be UTF-8.")?
                            .to_owned(),
                        InstructionFile {
                            text: rule.text,
                            mode: rule.mode,
                        },
                    ))
                })
                .collect::<Result<_>>()?,
        ))
    }

    pub fn instructions(&self) -> Result<Instructions> {
        let mut selected = BTreeMap::<PathBuf, (usize, Rule)>::new();
        for (path, file) in &self.0 {
            if !is_instruction(path) || !matches!(file.mode, 0o100644 | 0o100755) {
                return Err("Invalid frozen instruction file.".into());
            }
            let path = Path::new(path);
            let parent = path.parent().unwrap_or(Path::new(""));
            let scope = if matches!(
                parent.file_name().and_then(|name| name.to_str()),
                Some(".agents" | ".claude")
            ) {
                parent.parent().unwrap_or(Path::new(""))
            } else {
                parent
            };
            let priority = NAMES
                .iter()
                .position(|name| Path::new(name) == path.strip_prefix(scope).unwrap())
                .ok_or("Invalid instruction scope.")?;
            let sha256 = Sha256::digest(file.text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if selected.get(scope).is_none_or(|(old, _)| priority < *old) {
                selected.insert(
                    scope.to_path_buf(),
                    (
                        priority,
                        Rule {
                            path: path.into(),
                            scope: scope.into(),
                            sha256,
                            text: file.text.clone(),
                            mode: file.mode,
                        },
                    ),
                );
            }
        }
        Instructions::from_rules(selected.into_values().map(|(_, rule)| rule).collect())
    }
}

pub fn is_instruction(path: &str) -> bool {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path
        .components()
        .any(|part| !matches!(part, Component::Normal(name) if name != ".git" && name != ".intercat"))
    {
        return false;
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    match (
        parent.file_name().and_then(|name| name.to_str()),
        path.file_name().and_then(|name| name.to_str()),
    ) {
        (Some(".agents"), Some("AGENTS.md")) | (Some(".claude"), Some("CLAUDE.md")) => true,
        (Some(".agents" | ".claude"), _) => false,
        (_, Some("AGENTS.md" | "CLAUDE.md")) => true,
        _ => false,
    }
}

fn capture(root: &Path, selected_only: bool) -> Result<Vec<Rule>> {
    let root = root.canonicalize()?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&root)?;
    let mut rules = Vec::new();
    let mut total = 0;
    let walker = WalkDir::new(&root)
        .follow_links(false)
        .max_open(16)
        .into_iter()
        .filter_entry(|entry| {
            entry.path() == root
                || (entry.file_name() != ".git" && entry.file_name() != ".intercat")
        });
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_dir() {
            continue;
        }
        // .agents/AGENTS.md belongs to its parent scope, not a second nested scope.
        if entry.file_name() == ".agents" || entry.file_name() == ".claude" {
            continue;
        }
        for name in NAMES {
            let path = entry.path().join(name);
            let Some((bytes, mode)) = read_rule(&directory, path.strip_prefix(&root)?)? else {
                continue;
            };
            total += bytes.len();
            if total > TOTAL_LIMIT {
                return Err(std::io::Error::other("project instructions exceed 512 KiB").into());
            }
            let sha256 = Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            rules.push(Rule {
                path: path.strip_prefix(&root)?.to_path_buf(),
                scope: entry.path().strip_prefix(&root)?.to_path_buf(),
                sha256,
                text: String::from_utf8(bytes)?,
                mode,
            });
            if selected_only {
                break;
            }
        }
    }
    Ok(rules)
}

impl Instructions {
    pub fn capture(root: &Path) -> Result<Self> {
        Self::from_rules(capture(root, true)?)
    }
    fn from_rules(mut rules: Vec<Rule>) -> Result<Self> {
        rules.sort_by(|a, b| {
            a.scope
                .components()
                .count()
                .cmp(&b.scope.components().count())
                .then_with(|| a.path.cmp(&b.path))
        });
        let encoded = serde_json::to_vec(&rules)?;
        let digest = Sha256::digest(encoded)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self { rules, digest })
    }
    pub fn with_request(&self, request: &str) -> Result<String> {
        Ok(format!(
            "The following snapshot contains the complete current project instructions. It replaces previous project instructions, including any inherited from an older turn. An empty rules array means there are no current project instruction files; do not apply an older project's instruction snapshot. Apply these instructions to files in the stated directory and its descendants. An empty scope means the repository root. More specific directory instructions override broader instructions only within their scope. This replaces project instruction snapshots only; retain the user's requests and Intercat's reporting requirements.\n{}\n\nUser request:\n{}",
            serde_json::to_string_pretty(self)?,
            request
        ))
    }
}
