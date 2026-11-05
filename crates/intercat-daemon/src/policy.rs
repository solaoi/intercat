//! Deterministic rules at the host's approval boundary. Display text is never authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stops {
    #[default]
    IrreversibleOnly,
    EveryApproval,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    pub kind: String,
    pub message: String,
}
impl Reason {
    fn new(kind: &str, message: &str) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Impact {
    pub kind: String,
    pub target: String,
    pub rule_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub review: Option<Reason>,
    pub allow_once: bool,
    pub impact: Option<Impact>,
}
impl Verdict {
    fn clear() -> Self {
        Self {
            review: None,
            allow_once: true,
            impact: None,
        }
    }
    fn ask(kind: &str, message: &str, target: &str) -> Self {
        let impact = matches!(
            kind,
            "push" | "delete" | "migrate" | "deploy" | "send" | "external-write" | "schedule"
        )
        .then(|| Impact {
            kind: kind.into(),
            target: target.chars().take(200).collect(),
            rule_id: format!("{kind}.v1"),
        });
        Self {
            review: Some(Reason::new(kind, message)),
            allow_once: !matches!(kind, "schedule" | "detach"),
            impact,
        }
    }
}
pub struct Context<'a> {
    pub stops: Stops,
    pub workspace: &'a Path,
    pub recoverable_deletion: bool,
    pub remote_hosts: &'a [String],
}

const REGISTRIES: &[&str] = &[
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "formulae.brew.sh",
    "ghcr.io",
];
fn host_allowed(host: &str, remotes: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["localhost", "127.0.0.1", "::1", "[::1]"].contains(&host.as_str())
        || REGISTRIES.contains(&host.as_str())
        || remotes.iter().any(|h| h.eq_ignore_ascii_case(&host))
}
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && !['_', '-', '.'].contains(&c))
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}
fn has(words: &[String], word: &str) -> bool {
    words.iter().any(|s| s == word)
}
fn pair(words: &[String], a: &str, b: &str) -> bool {
    words.windows(2).any(|s| s[0] == a && s[1] == b)
}
fn basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}
pub(crate) fn git_subcommand(args: &[&str]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if [
            "-C",
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--config-env",
        ]
        .contains(&arg)
        {
            i += 2;
            continue;
        }
        if arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
            || arg.starts_with("--config-env=")
            || [
                "--no-pager",
                "--paginate",
                "--bare",
                "--no-optional-locks",
                "--literal-pathspecs",
                "--glob-pathspecs",
                "--noglob-pathspecs",
            ]
            .contains(&arg)
        {
            i += 1;
            continue;
        }
        return (!arg.starts_with('-')).then_some(i);
    }
    None
}
fn endpoint(args: &[&str]) -> String {
    args.iter()
        .find_map(|arg| {
            reqwest::Url::parse(arg).ok().and_then(|url| {
                url.host_str().map(|host| {
                    format!(
                        "{}://{}{}",
                        url.scheme(),
                        host,
                        url.port().map(|p| format!(":{p}")).unwrap_or_default()
                    )
                })
            })
        })
        .unwrap_or_else(|| "command destination".into())
}
fn shell(argv: &[&str], raw: &str, complete: bool, has_following_stage: bool) -> Verdict {
    if argv.is_empty() || !complete {
        let tokens = words(raw);
        if [
            "rm",
            "push",
            "eval",
            "sh",
            "bash",
            "zsh",
            "xargs",
            "source",
            "curl",
            "wget",
            "drop",
            "truncate",
            "migrate",
            "deploy",
            "nohup",
            "setsid",
            "crontab",
            "launchctl",
        ]
        .iter()
        .any(|s| has(&tokens, s))
            || raw.contains('&')
        {
            return Verdict::ask(
                "unparsed",
                "The command cannot be fully inspected before it runs.",
                "",
            );
        }
        return Verdict::clear();
    }
    let name = basename(argv[0]);
    let args = &argv[1..];
    let tokens = words(&args.join(" "));
    let wrapper = matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "xargs"
            | "eval"
            | "source"
            | "env"
            | "sudo"
            | "command"
            | "nice"
            | "timeout"
    );
    let is = |program: &str| name == program || wrapper && has(&tokens, program);
    let git_index = (name == "git").then(|| git_subcommand(args)).flatten();
    let git_is = |command: &str| {
        git_index.is_some_and(|index| args[index] == command)
            || wrapper && pair(&tokens, "git", command)
    };
    // The host provides argv for the current stage. For opaque wrappers these
    // are conservative literal rules, not a claim to parse arbitrary programs.
    if (is("at") || is("crontab"))
        || is("launchctl") && (has(&tokens, "load") || has(&tokens, "bootstrap"))
        || (is("sleep") && (has_following_stage || wrapper && raw.contains(';')))
    {
        return Verdict::ask(
            "schedule",
            "Scheduled jobs aren’t supported. Keep this work on the timeline.",
            name,
        );
    }
    if is("setsid")
        || is("nohup")
        || is("disown")
        || raw.trim_end().ends_with('&') && !raw.trim_end().ends_with("&&")
    {
        return Verdict::ask(
            "detach",
            "This command would detach work from Intercat’s controls.",
            name,
        );
    }
    if git_is("push") {
        let force = args
            .iter()
            .any(|s| *s == "--force" || *s == "-f" || s.starts_with("--force-with-lease"));
        return Verdict::ask(
            "push",
            if force {
                "This command can rewrite remote history."
            } else {
                "Pushing is a separate step after Commit."
            },
            git_index
                .and_then(|index| args.get(index + 1))
                .filter(|s| !s.starts_with('-') && !s.contains(':') && !s.contains('@'))
                .copied()
                .unwrap_or("remote"),
        );
    }
    if is("rm")
        || is("find") && has(&tokens, "-delete")
        || git_is("restore")
        || git_is("checkout") && args.contains(&"--")
        || git_is("reset") && has(&tokens, "--hard")
        || git_is("clean") && args.iter().any(|s| s.starts_with('-') && s.contains('f'))
        || git_is("stash") && (has(&tokens, "drop") || has(&tokens, "clear"))
    {
        return Verdict::ask(
            "delete",
            "This command discards files or local changes. A recoverable snapshot is required.",
            "files or local changes",
        );
    }
    let sql = matches!(
        name,
        "psql" | "mysql" | "mariadb" | "sqlite3" | "duckdb" | "sqlcmd"
    ) || wrapper && ["psql", "mysql", "sqlite3"].iter().any(|s| has(&tokens, s));
    let destructive_sql = args.join(" ").split(';').any(|statement| {
        let tokens = words(statement);
        has(&tokens, "drop")
            || has(&tokens, "truncate")
            || has(&tokens, "alter")
            || ((has(&tokens, "delete") || has(&tokens, "update")) && !has(&tokens, "where"))
    });
    let runner = wrapper
        || [
            "npm", "npx", "pnpm", "yarn", "bun", "prisma", "alembic", "rails", "rake", "python",
            "python3", "dotnet",
        ]
        .contains(&name);
    if is("dropdb")
        || is("redis-cli")
            && ["del", "unlink", "flushall", "flushdb"]
                .iter()
                .any(|s| has(&tokens, s))
        || sql && destructive_sql
        || is("mongosh")
            && tokens
                .iter()
                .any(|s| s == "drop" || s.ends_with(".drop") || s.ends_with(".dropdatabase"))
        || runner
            && (has(&tokens, "migrate")
                || args.iter().any(|s| matches!(*s, "db:drop" | "db:reset"))
                || args.contains(&"manage.py") && has(&tokens, "flush"))
        || is("docker")
            && (has(&tokens, "compose")
                && has(&tokens, "down")
                && (args.contains(&"-v") || args.contains(&"--volumes"))
                || pair(&tokens, "volume", "rm"))
    {
        return Verdict::ask(
            "migrate",
            "This command changes or removes stored data.",
            name,
        );
    }
    if (runner
        || [
            "aws",
            "gcloud",
            "fly",
            "firebase",
            "vercel",
            "netlify",
            "serverless",
        ]
        .contains(&name))
        && has(&tokens, "deploy")
        || is("terraform") && (has(&tokens, "apply") || has(&tokens, "destroy"))
        || is("kubectl")
            && [
                "apply", "create", "delete", "replace", "patch", "set", "rollout",
            ]
            .iter()
            .any(|s| has(&tokens, s))
        || is("helm")
            && ["install", "upgrade", "uninstall"]
                .iter()
                .any(|s| has(&tokens, s))
        || is("vercel") && !args.iter().any(|s| ["--help", "--version"].contains(s))
    {
        return Verdict::ask(
            "deploy",
            "This command changes a deployed environment.",
            name,
        );
    }
    let curl_write = is("curl")
        && args.iter().any(|s| {
            s.starts_with("--data")
                || s.starts_with("--json")
                || s.starts_with("--upload-file")
                || *s == "-T"
                || s.starts_with("-d")
                || s.starts_with("-F")
                || ["POST", "PUT", "PATCH", "DELETE"].iter().any(|verb| {
                    s.eq_ignore_ascii_case(verb)
                        || s.eq_ignore_ascii_case(&format!("-X{verb}"))
                        || s.eq_ignore_ascii_case(&format!("--request={verb}"))
                })
        });
    let wget_write = is("wget")
        && args.iter().any(|s| {
            s.starts_with("--post-")
                || s.starts_with("--method=POST")
                || s.starts_with("--method=PUT")
                || s.starts_with("--method=DELETE")
        });
    let other_write = matches!(name, "http" | "https" | "gh")
        && ["post", "put", "patch", "delete"]
            .iter()
            .any(|s| has(&tokens, s));
    if curl_write
        || wget_write
        || other_write
        || ["mail", "mailx", "sendmail", "msmtp", "swaks"]
            .iter()
            .any(|s| is(s))
    {
        return Verdict::ask(
            "send",
            "This command sends data outside this workspace.",
            &endpoint(args),
        );
    }
    if ["muse", "codex", "claude", "aider"].iter().any(|s| is(s)) {
        return Verdict::ask(
            "nested-agent",
            "This starts another agent outside the current track.",
            name,
        );
    }
    if wrapper {
        return Verdict::ask(
            "unparsed",
            "This wrapper can hide actions that need review.",
            "",
        );
    }
    Verdict::clear()
}

pub fn evaluate(request: &Value, context: &Context<'_>) -> Verdict {
    let subject = &request["subject"];
    let mut verdict = match subject["kind"].as_str() {
        Some("shell") => {
            let argv = request["currentRequirementId"]["sourceIndex"]
                .as_u64()
                .and_then(|index| subject["stages"].as_array()?.get(index as usize));
            let values = argv.and_then(|stage| stage["argv"].as_array());
            let strings = values
                .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let complete = argv.is_some_and(|stage| stage["argvComplete"] == true)
                && values.is_some_and(|values| values.len() == strings.len());
            let has_following_stage = request["currentRequirementId"]["sourceIndex"]
                .as_u64()
                .is_some_and(|index| {
                    subject["stages"].as_array().is_some_and(|stages| {
                        index
                            .checked_add(1)
                            .is_some_and(|next| next < stages.len() as u64)
                    }) || argv.is_some_and(|stage| {
                        stage["totalStages"]
                            .as_u64()
                            .zip(stage["position"].as_u64())
                            .is_some_and(|(total, position)| position < total)
                    })
                });
            if argv.is_none() {
                Verdict::ask(
                    "unparsed",
                    "This approval does not identify an available command stage.",
                    "",
                )
            } else {
                shell(
                    &strings,
                    subject["command"].as_str().unwrap_or(""),
                    complete,
                    has_following_stage,
                )
            }
        }
        Some("network") => {
            let host = subject["host"].as_str().unwrap_or("");
            if host_allowed(host, context.remote_hosts) {
                Verdict::clear()
            } else {
                Verdict::ask(
                    "send",
                    "This destination is outside the allowed hosts.",
                    &format!(
                        "{}{}",
                        host,
                        subject["port"]
                            .as_u64()
                            .map(|p| format!(":{p}"))
                            .unwrap_or_default()
                    ),
                )
            }
        }
        Some("fileAccess") => {
            let path = Path::new(subject["path"].as_str().unwrap_or(""));
            let write = subject["access"] == "write";
            if write
                && (path.is_absolute() && !path.starts_with(context.workspace)
                    || path.components().any(|p| p == Component::ParentDir))
            {
                Verdict::ask(
                    "external-write",
                    "This action writes outside the worker workspace.",
                    &path.to_string_lossy(),
                )
            } else if write
                && (path.components().any(|p| p.as_os_str() == ".git")
                    || intercat_git::secret_path(&path.to_string_lossy()))
            {
                Verdict::ask("write", "This action writes to a protected location.", "")
            } else {
                Verdict::clear()
            }
        }
        _ => Verdict::ask("unknown", "This approval needs your review.", ""),
    };
    if context.recoverable_deletion
        && verdict
            .impact
            .as_ref()
            .is_some_and(|impact| impact.kind == "delete")
    {
        verdict.review = None;
        verdict.impact = None;
    }
    let elevated = request["rawArgs"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .is_some_and(|a| a["sandbox_permissions"] == "require_escalated");
    if elevated {
        verdict.review = Some(Reason::new(
            "escalation",
            "This command would run outside the worker sandbox.",
        ));
    } else if request["protectedWrite"] == true {
        if subject["kind"] == "fileAccess" {
            verdict.impact = None;
        }
        verdict.review = Some(Reason::new(
            "write",
            "This action writes to a protected location.",
        ));
    }
    if verdict.review.is_none() && context.stops == Stops::EveryApproval {
        verdict.review = Some(Reason::new(
            "approval",
            "Review this action before it runs.",
        ));
    }
    verdict
}
