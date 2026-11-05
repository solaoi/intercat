//! Project rules are explicitly scoped when the native host runs without project trust.
use crate::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
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

#[derive(Debug, Serialize)]
pub struct Rule {
    pub path: PathBuf,
    pub scope: PathBuf,
    pub sha256: String,
    pub text: String,
}
#[derive(Debug, Serialize)]
pub struct Instructions {
    pub rules: Vec<Rule>,
    pub digest: String,
}

impl Instructions {
    pub fn capture(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
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
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                    Ok(metadata) if !metadata.is_file() => {
                        return Err(std::io::Error::other(
                            "instruction file must be a regular file, not a symlink",
                        )
                        .into());
                    }
                    Ok(_) => {}
                }
                let mut bytes = Vec::new();
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)?
                    .take(FILE_LIMIT + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > FILE_LIMIT {
                    return Err(std::io::Error::other("instruction file exceeds 64 KiB").into());
                }
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
                });
                break;
            }
        }
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
        if self.rules.is_empty() {
            return Ok(request.into());
        }
        Ok(format!(
            "Apply these project instructions to files in the stated directory and its descendants. An empty scope means the repository root. More specific directory instructions override broader instructions only within their scope.\n{}\n\nUser request:\n{}",
            serde_json::to_string_pretty(self)?,
            request
        ))
    }
}
