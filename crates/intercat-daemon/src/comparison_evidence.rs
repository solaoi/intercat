//! Test and runner changes from frozen Git blobs. No project code is executed.
use crate::{
    Result,
    comparison::Job,
    integration::{Change, File},
};
use intercat_git::{Repository, SavedFile};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub added: usize,
    pub deleted: usize,
    pub modified: usize,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Side {
    pub tests: Counts,
    pub skip_added: usize,
    pub runner: Vec<String>,
    pub unreadable: Vec<String>,
    pub command_unparsed: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub id: uuid::Uuid,
    pub sides: [Side; 2],
    pub changes: Vec<Change>,
}
fn test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|part| matches!(part, "test" | "tests" | "__tests__"))
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name.starts_with("test_") && name.ends_with(".py")
        || name == "conftest.py"
}
fn configuration(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    [
        "jest.config.",
        "vitest.config.",
        "vite.config.",
        "playwright.config.",
        ".mocharc.",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || matches!(
            name,
            ".mocharc" | "pytest.ini" | "tox.ini" | "setup.cfg" | "pyproject.toml"
        )
}
fn bytes(repo: &Repository, file: Option<&File>, remaining: &mut usize) -> Result<Option<Vec<u8>>> {
    let Some(file) = file else {
        return Ok(Some(vec![]));
    };
    let saved = SavedFile {
        object: file.object.parse()?,
        mode: file.mode,
    };
    let size = repo.saved_size(&saved)?;
    if !matches!(saved.mode, 0o100644 | 0o100755) || size > 1024 * 1024 || size > *remaining as u64
    {
        return Ok(None);
    }
    let bytes = repo.saved_bytes(&saved)?;
    *remaining -= bytes.len();
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        Ok(None)
    } else {
        Ok(Some(bytes))
    }
}
fn manifest_fields(bytes: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = if bytes.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(bytes)?
    };
    Ok(
        serde_json::json!({"scripts":value.get("scripts"),"jest":value.get("jest"),"mocha":value.get("mocha"),"ava":value.get("ava")}),
    )
}
fn named_by_command(path: &str, words: &[String]) -> bool {
    if matches!(
        path.rsplit('/').next(),
        Some("Makefile" | "makefile" | "GNUmakefile")
    ) && words
        .iter()
        .any(|word| matches!(word.rsplit('/').next(), Some("make" | "gmake")))
    {
        return true;
    }
    words.iter().any(|word| {
        word.split('=').any(|part| {
            part.trim_matches([';', '|', '&', '(', ')'])
                .trim_start_matches("./")
                == path
        })
    })
}
pub(crate) fn inspect(
    repo: &Repository,
    changes: &[Change],
    command: Option<&str>,
) -> Result<Side> {
    let mut result = Side::default();
    let words = command.map(shlex::split);
    result.command_unparsed = matches!(words, Some(None));
    let words = words.flatten().unwrap_or_default();
    let skip = regex::Regex::new(
        r"(?x)(?:\.(?:skip|todo)\s*\(|\b(?:xit|xtest|xdescribe|fdescribe|fit|pending)\s*\(|@pytest\.mark\.(?:skip|skipif|xfail)\b|@unittest\.(?:skip|skipIf|skipUnless|expectedFailure)\b|\bt\.(?:Skip|Skipf|SkipNow)\s*\()",
    )?;
    let mut runners = BTreeSet::new();
    let mut remaining = 64 * 1024 * 1024;
    for change in changes {
        if test_path(&change.path) {
            match (&change.before, &change.after) {
                (None, Some(_)) => result.tests.added += 1,
                (Some(_), None) => result.tests.deleted += 1,
                _ => result.tests.modified += 1,
            }
        }
        let runner = configuration(&change.path) || named_by_command(&change.path, &words);
        if runner {
            runners.insert(change.path.clone());
        }
        // Scan changed text for newly added skip markers, including inline tests.
        let before = bytes(repo, change.before.as_ref(), &mut remaining)?;
        let after = bytes(repo, change.after.as_ref(), &mut remaining)?;
        let (Some(before), Some(after)) = (before, after) else {
            result.unreadable.push(change.path.clone());
            continue;
        };
        let lines = std::str::from_utf8(&after)?.lines().collect::<Vec<_>>();
        for range in intercat_git::added_lines(&before, &after) {
            for index in range {
                if let Some(line) = lines.get(index as usize) {
                    result.skip_added += skip.find_iter(line).count();
                }
            }
        }
        if change.path.rsplit('/').next() == Some("package.json") {
            match (manifest_fields(&before), manifest_fields(&after)) {
                (Ok(before), Ok(after)) if before != after => {
                    runners.insert(format!("{} scripts / runner settings", change.path));
                }
                (Err(_), _) | (_, Err(_)) => result.unreadable.push(change.path.clone()),
                _ => {}
            }
        }
    }
    result.runner = runners.into_iter().collect();
    result.unreadable.sort();
    result.unreadable.dedup();
    Ok(result)
}
pub(crate) fn read(job: &Job, choice: &crate::check_run::CheckCommand) -> Result<Evidence> {
    let repo = Repository::open(&job.folder.root)?;
    let inspect_side = |side: &crate::comparison::Side| -> Result<Side> {
        match crate::check_run::resolve_command(&repo, side.tree.parse()?, choice) {
            Ok(command) => inspect(&repo, &side.changes, command.as_deref()),
            Err(_) => {
                let mut result = inspect(&repo, &side.changes, None)?;
                result.command_unparsed = true;
                Ok(result)
            }
        }
    };
    Ok(Evidence {
        id: job.id,
        sides: [inspect_side(&job.sides[0])?, inspect_side(&job.sides[1])?],
        changes: crate::integration::changes(
            &repo,
            job.sides[0].tree.parse()?,
            job.sides[1].tree.parse()?,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, process::Command};
    fn git(path: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("/usr/bin/git")
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "init.templateDir=/dev/null",
                "-c",
                "user.name=Own evidence fixture",
                "-c",
                "user.email=own@example.invalid",
            ])
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().into())
    }
    fn own() -> Result<tempfile::TempDir> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        git(root.path(), &["init", "-q", "-b", "main"])?;
        Ok(root)
    }
    fn write(root: &Path, path: &str, text: &str) -> Result<()> {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, text)?;
        Ok(())
    }
    fn save(root: &Path) -> Result<intercat_git::ObjectId> {
        git(root, &["add", "-A"])?;
        git(root, &["commit", "-qm", "Own evidence snapshot"])?;
        Ok(git(root, &["rev-parse", "HEAD^{tree}"])?.parse()?)
    }
    #[test]
    fn runner_changes_and_new_skips_are_distinct_from_result_failures() -> Result<()> {
        let own = own()?;
        let root = own.path();
        write(
            root,
            "package.json",
            r#"{"scripts":{"test":"vitest run","helper":"exit 1"},"dependencies":{"own":"1"}}"#,
        )?;
        write(
            root,
            "tests/old.spec.ts",
            "test.skip('existing', () => {});\n",
        )?;
        write(root, "tests/gone.spec.ts", "test('old', () => {});\n")?;
        write(root, "src/core.ts", "export const own = 1;\n")?;
        write(root, "scripts/test.sh", "exit 1\n")?;
        write(root, "Makefile", "test:\n\texit 1\n")?;
        write(
            root,
            "vite.config.ts",
            "export default {test: {include: ['tests/*']}};\n",
        )?;
        let before = save(root)?;
        write(
            root,
            "package.json",
            r#"{"scripts":{"test":"vitest run","helper":"exit 0","pretest":"true"},"dependencies":{"own":"1"}}"#,
        )?;
        write(
            root,
            "tests/old.spec.ts",
            "test.skip('existing', () => {});\ntest('new assertion', () => {});\n",
        )?;
        fs::remove_file(root.join("tests/gone.spec.ts"))?;
        write(
            root,
            "tests/new.spec.ts",
            "test.skip('newly skipped', () => {});\n",
        )?;
        write(
            root,
            "src/core.ts",
            "export const own = 1;\nt.Skip(\"own inline marker\");\n",
        )?;
        write(root, "scripts/test.sh", "exit 0\n")?;
        write(root, "Makefile", "test:\n\texit 0\n")?;
        write(
            root,
            "vite.config.ts",
            "export default {test: {exclude: ['tests/*']}};\n",
        )?;
        let after = save(root)?;
        let repo = Repository::open(root)?;
        let evidence = inspect(
            &repo,
            &crate::integration::changes(&repo, before, after)?,
            Some("make test && './scripts/test.sh'"),
        )?;
        assert_eq!(
            (
                evidence.tests.added,
                evidence.tests.deleted,
                evidence.tests.modified
            ),
            (1, 1, 1)
        );
        assert_eq!(evidence.skip_added, 2);
        for label in [
            "package.json scripts / runner settings",
            "Makefile",
            "scripts/test.sh",
            "vite.config.ts",
        ] {
            assert!(evidence.runner.contains(&label.into()), "{evidence:?}");
        }
        assert!(evidence.unreadable.is_empty());
        assert!(!evidence.command_unparsed);
        Ok(())
    }
    #[test]
    fn dependency_only_manifests_do_not_claim_the_runner_changed() -> Result<()> {
        let own = own()?;
        let root = own.path();
        write(
            root,
            "package.json",
            r#"{"scripts":{"test":"vitest run"},"jest":{"testMatch":["tests/*"]},"dependencies":{"own":"1"}}"#,
        )?;
        let before = save(root)?;
        write(
            root,
            "package.json",
            r#"{"dependencies":{"own":"2"},"scripts":{"test":"vitest run"},"jest":{"testMatch":["tests/*"]}}"#,
        )?;
        write(root, "sub/package.json", r#"{"dependencies":{"own":"3"}}"#)?;
        let after = save(root)?;
        let repo = Repository::open(root)?;
        let evidence = inspect(
            &repo,
            &crate::integration::changes(&repo, before, after)?,
            Some("npm test"),
        )?;
        assert!(evidence.runner.is_empty(), "{evidence:?}");
        assert!(evidence.unreadable.is_empty());
        Ok(())
    }
    #[test]
    fn python_js_conventions_and_unreadable_files_remain_visible() -> Result<()> {
        let own = own()?;
        let root = own.path();
        write(root, "own.txt", "Own base\n")?;
        let before = save(root)?;
        for path in [
            "test/unit.py",
            "__tests__/one.ts",
            "src/core_test.go",
            "src/test_own.py",
            "conftest.py",
        ] {
            write(root, path, "@pytest.mark.skip\ndef own(): pass\n")?;
        }
        for path in [
            "jest.config.cjs",
            "vitest.config.ts",
            ".mocharc.json",
            "playwright.config.ts",
            "pytest.ini",
            "tox.ini",
            "setup.cfg",
            "pyproject.toml",
        ] {
            write(root, path, "Own runner config\n")?;
        }
        fs::write(root.join("own-binary.dat"), [0, 1, 2])?;
        fs::write(root.join("own-large.txt"), vec![b'x'; 1024 * 1024 + 1])?;
        let after = save(root)?;
        let repo = Repository::open(root)?;
        let evidence = inspect(
            &repo,
            &crate::integration::changes(&repo, before, after)?,
            Some("./scripts/test.sh '"),
        )?;
        assert_eq!(evidence.tests.added, 5);
        assert_eq!(evidence.skip_added, 5);
        assert_eq!(evidence.runner.len(), 8);
        assert_eq!(evidence.unreadable, vec!["own-binary.dat", "own-large.txt"]);
        assert!(evidence.command_unparsed);
        Ok(())
    }
}
