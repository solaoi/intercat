//! Pinned Gitleaks rules and exact known values. Scan Git object bytes, never a
//! mutable worktree. Only locations and fingerprints leave the private scanner.
use crate::{Result, command};
use intercat_git::{ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    ops::Range,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

pub const VERSION: &str = "8.30.1";
pub const BINARY_REVIEW_RULE: &str = "binary-pattern-scan-incomplete";
const INPUT_LIMIT: usize = 64 * 1024 * 1024;
const FINDING_LIMIT: usize = 2048;
pub(crate) const CONFIG: &str = include_str!("../rules/gitleaks-8.30.1.toml");
pub fn ruleset() -> String {
    format!(
        "gitleaks-{VERSION}/{};config={};input-v2-lfs",
        binary_hash(),
        digest(CONFIG.as_bytes())
    )
}

fn binary_hash() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f"
    } else {
        "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291"
    }
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn value_fingerprints(values: &[String]) -> Vec<String> {
    values
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|value| digest(value.as_bytes()))
        .collect()
}
fn verify(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err("Gitleaks must be a regular file.".into());
    }
    let mut sha = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        sha.update(&buffer[..count]);
    }
    if sha
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        != binary_hash()
    {
        return Err("Gitleaks integrity check failed. Run npm run setup:tools.".into());
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Engine {
    executable: PathBuf,
}
impl Engine {
    pub fn capture() -> Result<Self> {
        Self::open(
            std::env::var_os("INTERCAT_GITLEAKS")
                .ok_or("Gitleaks is not configured. Run npm run setup:tools.")?
                .into(),
        )
    }
    pub fn open(path: PathBuf) -> Result<Self> {
        let executable = path.canonicalize()?;
        verify(&executable)?;
        Ok(Self { executable })
    }
    pub fn start(&self, known_values: &[String]) -> Result<Scanner> {
        let root = tempfile::Builder::new()
            .prefix("intercat-scan-")
            .tempdir()?;
        self.prepare(root, known_values)
    }
    pub(crate) fn start_in(&self, parent: &Path, known_values: &[String]) -> Result<Scanner> {
        let root = tempfile::Builder::new()
            .prefix("intercat-scan-")
            .tempdir_in(parent)?;
        self.prepare(root, known_values)
    }
    fn prepare(&self, root: tempfile::TempDir, known_values: &[String]) -> Result<Scanner> {
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let executable = root.path().join("gitleaks");
        fs::copy(&self.executable, &executable)?;
        verify(&executable)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o500))?;
        fs::write(root.path().join("rules.toml"), CONFIG)?;
        fs::write(root.path().join("ignore"), "")?;
        let mut values = known_values
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        if values.len() > 256 || values.iter().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err("Too many known secret values to scan safely.".into());
        }
        Ok(Scanner { root, values })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub commit: String,
    pub parent_tree: String,
    pub tree: String,
    pub message_hash: String,
    pub ruleset: String,
    pub value_fingerprints: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    pub rule: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    pub binding: Binding,
    pub findings: Vec<Finding>,
    pub unscanned_lfs: Vec<String>,
}
impl Scan {
    pub fn accepted(&self, kept: &std::collections::BTreeSet<String>) -> bool {
        self.findings
            .iter()
            .all(|finding| kept.contains(&finding.id))
    }
    pub(crate) fn review_reason(
        &self,
        kept: &std::collections::BTreeSet<String>,
        secret_reason: &'static str,
    ) -> Option<&'static str> {
        if self.accepted(kept) {
            None
        } else if self
            .findings
            .iter()
            .any(|finding| finding.rule != BINARY_REVIEW_RULE && !kept.contains(&finding.id))
        {
            Some(secret_reason)
        } else {
            Some("Binary pattern scan incomplete · review before committing.")
        }
    }
    pub fn fingerprint(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}

struct Fragment {
    path: String,
    bytes: Vec<u8>,
    added: Vec<Range<u32>>,
}
impl Fragment {
    fn all(path: String, bytes: Vec<u8>) -> Self {
        Self {
            path,
            bytes,
            added: std::iter::once(0..u32::MAX).collect(),
        }
    }
    fn touches(&self, start: u32, end: u32) -> bool {
        self.added
            .iter()
            .any(|range| range.start < end && start.saturating_sub(1) < range.end)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Detected {
    file: String,
    start_line: u32,
    end_line: u32,
    start_column: usize,
    end_column: usize,
    #[serde(rename = "Match")]
    redacted_match: String,
    #[serde(rename = "RuleID")]
    rule: String,
}

enum CommitScope {
    Candidate(ObjectId),
    Push,
}

pub struct Scanner {
    root: tempfile::TempDir,
    // Never serialized, logged, put in argv, or written to a config file.
    values: Vec<String>,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str().ok_or("Invalid scanner path.")?,
    )?)
}
impl Scanner {
    fn ruleset(&self) -> String {
        ruleset()
    }
    async fn detect(
        &self,
        fragments: &[Fragment],
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<(usize, Detected)>> {
        if fragments.is_empty() {
            return Ok(vec![]);
        }
        let input = tempfile::Builder::new()
            .prefix("input-")
            .tempdir_in(self.root.path())?;
        let mut mapping = BTreeMap::new();
        let mut total = 0usize;
        for (index, fragment) in fragments.iter().enumerate() {
            total = total
                .checked_add(fragment.bytes.len())
                .ok_or("Secret scan input is too large.")?;
            if total > INPUT_LIMIT {
                return Err("Secret scan input exceeds 64 MiB.".into());
            }
            // Preserve the repository name for path-based rules, but materialize
            // symlink blobs as ordinary bytes. No source path is ever followed.
            let relative = Path::new(&fragment.path);
            if relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err("Unsafe path in secret scan.".into());
            }
            let file = input.path().join(relative);
            fs::create_dir_all(file.parent().ok_or("Missing scan parent.")?)?;
            fs::write(&file, &fragment.bytes)?;
            mapping.insert(file.to_string_lossy().into_owned(), index);
        }
        let root = self.root.path().canonicalize()?;
        let executable = root.join("gitleaks");
        let mut profile = format!(
            "(version 1)(deny default)(allow process-info* sysctl-read mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (subpath {}) (subpath \"/System\") (subpath \"/usr/lib\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/dev\")) (allow process-exec (literal {}))(allow file-write* (literal \"/dev/null\"))",
            quote(&root)?,
            quote(&executable)?
        );
        for parent in root.ancestors() {
            profile.push_str(&format!("(allow file-read* (literal {}))", quote(parent)?));
        }
        let mut command = command::gated_command("/usr/bin/sandbox-exec", None);
        command
            .args(["-p", &profile])
            .arg(&executable)
            .args([
                "dir",
                "--no-banner",
                "--no-color",
                "--redact=100",
                "--ignore-gitleaks-allow",
                "--log-level=error",
                "--report-format=json",
                "--report-path=-",
                "--exit-code=10",
                "--timeout=30",
                "--config",
            ])
            .arg(root.join("rules.toml"))
            .arg("--gitleaks-ignore-path")
            .arg(root.join("ignore"))
            .arg(input.path())
            .current_dir(&root)
            .env("HOME", &root)
            .env("TMPDIR", &root);
        let started = std::time::Instant::now();
        let output = command::run(&mut command, cancel, Duration::from_secs(35)).await?;
        if output.cancelled {
            return Err("Secret scan cancelled.".into());
        }
        // Warnings/errors can mean an unreadable fragment or skipped input. Do
        // not accept a partial scan, malformed report, or truncated report.
        if output.timed_out
            || output.truncated
            || ![Some(0), Some(10)].contains(&output.code)
            || !output.stderr.is_empty()
        {
            return Err(format!(
                "Secret scan did not complete. No scan result was accepted. elapsedMs={} timeout={} truncated={} exit={:?} diagnostics={}",
                started.elapsed().as_millis(), output.timed_out, output.truncated,
                output.code, !output.stderr.is_empty(),
            ).into());
        }
        let detected: Vec<Detected> =
            serde_json::from_slice(&output.stdout).map_err(|_| "Invalid secret scan report.")?;
        if detected.len() > FINDING_LIMIT {
            return Err("Too many secret findings; no partial result was accepted.".into());
        }
        detected
            .into_iter()
            .map(|item| {
                let index = *mapping
                    .get(&item.file)
                    .ok_or("Secret scanner reported an unknown path.")?;
                if item.start_line == 0 || item.end_line < item.start_line {
                    return Err("Invalid secret scan location.".into());
                }
                Ok((index, item))
            })
            .collect()
    }

    async fn fragments(
        &self,
        fragments: &[Fragment],
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        for (index, item) in self.detect(fragments, cancel).await? {
            let fragment = &fragments[index];
            if fragment.touches(item.start_line, item.end_line) {
                findings.push(Finding {
                    id: String::new(),
                    path: fragment.path.clone(),
                    line: item.start_line,
                    end_line: item.end_line,
                    rule: item.rule,
                });
            }
        }
        for fragment in fragments {
            // Exact values also scan binary bytes, independent of Gitleaks's
            // supported encodings and its default rule allowlists.
            for value in &self.values {
                let mut previous = 0;
                let mut line = 1;
                let newlines = value.bytes().filter(|byte| *byte == b'\n').count() as u32;
                for (offset, _) in fragment
                    .bytes
                    .windows(value.len())
                    .enumerate()
                    .filter(|(_, bytes)| *bytes == value.as_bytes())
                {
                    line += fragment.bytes[previous..offset]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count() as u32;
                    previous = offset;
                    let end = line + newlines;
                    if fragment.touches(line, end) {
                        findings.push(Finding {
                            id: String::new(),
                            path: fragment.path.clone(),
                            line,
                            end_line: end,
                            rule: "check-environment-value".into(),
                        });
                        if findings.len() > FINDING_LIMIT {
                            return Err(
                                "Too many secret findings; no partial result was accepted.".into(),
                            );
                        }
                    }
                }
            }
        }
        findings.sort_by(|a, b| {
            (&a.path, a.line, a.end_line, &a.rule).cmp(&(&b.path, b.line, b.end_line, &b.rule))
        });
        findings.dedup();
        Ok(findings)
    }

    pub async fn commit(
        &self,
        repository: &Repository,
        parent: ObjectId,
        commit: ObjectId,
        lfs_storage: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<Scan> {
        self.scan_commit(
            repository,
            commit,
            CommitScope::Candidate(parent),
            lfs_storage,
            cancel,
        )
        .await
    }

    /// Inspect every newly transmitted commit, including roots and merge
    /// commits. The caller chooses the range from the remote's advertised refs.
    pub async fn push_commit(
        &self,
        repository: &Repository,
        commit: ObjectId,
        lfs_storage: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<Scan> {
        self.scan_commit(repository, commit, CommitScope::Push, lfs_storage, cancel)
            .await
    }

    async fn scan_commit(
        &self,
        repository: &Repository,
        commit: ObjectId,
        scope: CommitScope,
        lfs_storage: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<Scan> {
        let details = repository.commit_details(commit)?;
        let parent_tree = match scope {
            CommitScope::Candidate(parent) => {
                if details.parents != vec![parent] {
                    return Err("Secret scan requires the exact candidate parent.".into());
                }
                repository.tree_id(parent)?
            }
            CommitScope::Push => match details.parents.first() {
                Some(parent) => repository.tree_id(*parent)?,
                None => repository.write_tree(&[])?,
            },
        };
        let files = |tree| {
            if matches!(scope, CommitScope::Push) {
                repository.push_tree_files(tree)
            } else {
                repository.tree_files(tree)
            }
        };
        let before = files(parent_tree)?;
        let after = files(details.tree)?;
        let binding = Binding {
            commit: commit.to_string(),
            parent_tree: parent_tree.to_string(),
            tree: details.tree.to_string(),
            message_hash: digest(details.message.as_bytes()),
            ruleset: self.ruleset(),
            value_fingerprints: value_fingerprints(&self.values),
        };
        let mut fragments = vec![Fragment::all(
            "commit-message.txt".into(),
            details.message.into_bytes(),
        )];
        let mut findings = Vec::new();
        let mut unscanned_lfs = Vec::new();
        let mut input_bytes = fragments[0].bytes.len() as u64;
        for (path, file) in &after {
            if before
                .get(path)
                .is_some_and(|old| old.object == file.object)
            {
                continue;
            }
            input_bytes = input_bytes.saturating_add(repository.saved_size(file)?);
            if let Some(old) = before.get(path) {
                input_bytes = input_bytes.saturating_add(repository.saved_size(old)?);
            }
            if input_bytes > INPUT_LIMIT as u64 {
                return Err("Secret scan input exceeds 64 MiB.".into());
            }
            let old = before
                .get(path)
                .map(|file| repository.saved_bytes(file))
                .transpose()?
                .unwrap_or_default();
            let bytes = repository.saved_bytes(file)?;
            let pointer = matches!(file.mode, 0o100644 | 0o100755)
                .then(|| intercat_git::lfs::Pointer::parse(&bytes))
                .flatten();
            let old_pointer = before
                .get(path)
                .filter(|file| matches!(file.mode, 0o100644 | 0o100755))
                .and_then(|_| intercat_git::lfs::Pointer::parse(&old));
            let old = self.lfs_bytes(path, &old, old_pointer, lfs_storage, &mut input_bytes)?;
            let is_lfs = pointer.is_some();
            let bytes = self.lfs_bytes(path, &bytes, pointer, lfs_storage, &mut input_bytes)?;
            if intercat_git::secret_path(path) && !before.contains_key(path) {
                findings.push(Finding {
                    id: String::new(),
                    path: path.clone(),
                    line: 1,
                    end_line: 1,
                    rule: "secret-file".into(),
                });
            }
            if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                if is_lfs {
                    unscanned_lfs.push(path.clone());
                } else {
                    findings.push(Finding {
                        id: String::new(),
                        path: path.clone(),
                        line: 1,
                        end_line: 1,
                        rule: BINARY_REVIEW_RULE.into(),
                    });
                }
                // Exact Check-environment values still scan every byte. The
                // UI explicitly states that binary rule scanning is incomplete.
                fragments.push(Fragment::all(format!("files/{path}"), bytes));
                continue;
            }
            fragments.push(Fragment {
                path: format!("files/{path}"),
                added: intercat_git::added_lines(&old, &bytes),
                bytes,
            });
        }
        findings.extend(
            self.fragments(&fragments, cancel)
                .await?
                .into_iter()
                .map(|mut item| {
                    if let Some(path) = item.path.strip_prefix("files/") {
                        item.path = path.to_string();
                    }
                    item
                }),
        );
        let seed = serde_json::to_vec(&binding)?;
        for finding in &mut findings {
            let mut key = seed.clone();
            key.extend(serde_json::to_vec(&finding)?);
            finding.id = digest(&key);
        }
        Ok(Scan {
            binding,
            findings,
            unscanned_lfs,
        })
    }

    fn lfs_bytes(
        &self,
        path: &str,
        git_bytes: &[u8],
        pointer: Option<intercat_git::lfs::Pointer>,
        storage: &Path,
        input_bytes: &mut u64,
    ) -> Result<Vec<u8>> {
        let Some(pointer) = pointer else {
            return Ok(git_bytes.to_vec());
        };
        *input_bytes = input_bytes.saturating_add(pointer.size());
        if *input_bytes > INPUT_LIMIT as u64 {
            return Err("Secret scan input exceeds 64 MiB.".into());
        }
        let entry = intercat_git::lfs::Entry {
            path: path.into(),
            pointer,
        };
        let Some(mut file) = crate::lfs_store::read(storage, &entry)? else {
            return Ok(Vec::new());
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take(entry.pointer.size() + 1)
            .read_to_end(&mut bytes)?;
        // Revalidate after reading. A mutable checkout cannot substitute
        // unscanned bytes while retaining the pointer recorded in the binding.
        if bytes.len() as u64 != entry.pointer.size() || digest(&bytes) != entry.pointer.oid() {
            return Err(crate::lfs_store::Problem::Corrupt { path: path.into() }.into());
        }
        Ok(bytes)
    }

    pub(crate) async fn private_spans(
        &self,
        texts: &[String],
    ) -> Result<Vec<Vec<crate::privacy::Span>>> {
        let fragments = texts
            .iter()
            .enumerate()
            .map(|(index, text)| Fragment::all(format!("{index}.txt"), text.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let mut spans = vec![Vec::new(); texts.len()];
        let mut lines = BTreeMap::<usize, Vec<usize>>::new();
        for (index, item) in self.detect(&fragments, watch::channel(false).1).await? {
            let text = &texts[index];
            // Gitleaks columns count bytes from the preceding newline, not after it.
            let offsets = lines.entry(index).or_insert_with(|| {
                std::iter::once(0)
                    .chain(text.match_indices('\n').map(|(offset, _)| offset))
                    .collect()
            });
            let base = |line: u32| -> Result<usize> {
                offsets
                    .get(line as usize - 1)
                    .copied()
                    .ok_or_else(|| "Invalid secret scan line.".into())
            };
            let start = base(item.start_line)?
                .checked_add(
                    item.start_column
                        .checked_sub(1)
                        .ok_or("Invalid secret scan column.")?,
                )
                .ok_or("Invalid secret scan column.")?;
            let end = base(item.end_line)?
                .checked_add(item.end_column)
                .ok_or("Invalid secret scan column.")?;
            let matched = text.get(start..end).ok_or("Invalid secret scan span.")?;
            for range in private_ranges(matched, &item.redacted_match)? {
                spans[index].push(crate::privacy::Span {
                    start: start + range.start,
                    end: start + range.end,
                    kind: if item.rule.contains("token") {
                        crate::privacy::Kind::Token
                    } else {
                        crate::privacy::Kind::ApiKey
                    },
                });
            }
        }
        Ok(spans)
    }

    /// Hide complete matching lines, including multiline matches, before a
    /// message or subprocess output can enter the journal or reach a worker.
    pub async fn redact(
        &self,
        texts: &[String],
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<String>> {
        let fragments = texts
            .iter()
            .enumerate()
            .map(|(i, text)| Fragment::all(format!("{i}.txt"), text.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let findings = self.fragments(&fragments, cancel).await?;
        Ok(texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let path = format!("{index}.txt");
                text.split_inclusive('\n')
                    .enumerate()
                    .map(|(line, text)| {
                        if findings.iter().any(|finding| {
                            finding.path == path
                                && finding.line as usize <= line + 1
                                && line < finding.end_line as usize
                        }) {
                            if text.ends_with('\n') {
                                "[redacted]\n"
                            } else {
                                "[redacted]"
                            }
                        } else {
                            text
                        }
                    })
                    .collect()
            })
            .collect())
    }
}

// Reconstruct only the captured value from a report that has already hidden it.
// Literal occurrences of the word REDACTED in a field name are kept intact.
fn private_ranges(raw: &str, masked: &str) -> Result<Vec<Range<usize>>> {
    let markers = masked
        .match_indices("REDACTED")
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    if markers.is_empty() || markers.len() > 256 || raw == masked {
        return Err("Invalid redacted secret match.".into());
    }
    for at in &markers {
        if !raw.starts_with(&masked[..*at]) {
            continue;
        }
        for count in 1..=markers.len() {
            let Some(bytes) = raw
                .len()
                .checked_add(count * "REDACTED".len())
                .and_then(|size| size.checked_sub(masked.len()))
            else {
                continue;
            };
            if bytes == 0 || bytes % count != 0 {
                continue;
            }
            let Some(value) = raw.get(*at..*at + bytes / count) else {
                continue;
            };
            if raw.replace(value, "REDACTED") != masked {
                continue;
            }
            let spans = raw
                .match_indices(value)
                .map(|(offset, found)| offset..offset + found.len())
                .collect::<Vec<_>>();
            if spans.len() == count {
                return Ok(spans);
            }
        }
    }
    Err("Secret scan match changed.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_values_preserve_literal_redacted_words_and_repeated_matches() -> Result<()> {
        assert_eq!(
            private_ranges("REDACTED_TOKEN=private-value", "REDACTED_TOKEN=REDACTED")?,
            vec![15..28]
        );
        assert_eq!(
            private_ranges("key=value:value", "key=REDACTED:REDACTED")?,
            vec![4..9, 10..15]
        );
        assert!(private_ranges("changed value", "key=REDACTED").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn pasted_text_scan_files_stay_in_the_private_data_directory() -> Result<()> {
        let data = tempfile::tempdir()?;
        let scanner = Engine::capture()?.start_in(data.path(), &[])?;
        assert!(scanner.root.path().starts_with(data.path()));
        assert_eq!(
            fs::metadata(scanner.root.path())?.permissions().mode() & 0o777,
            0o700
        );
        let texts = vec!["A private pasted document.".to_owned()];
        assert_eq!(
            scanner.redact(&texts, watch::channel(false).1).await?,
            texts
        );
        drop(scanner);
        assert_eq!(fs::read_dir(data.path())?.count(), 0);
        Ok(())
    }
}
