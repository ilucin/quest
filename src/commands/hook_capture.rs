//! `q hook post-tool-use` — auto-capture of links from Claude Code's
//! `PostToolUse` payload (SPEC §12): PR and task URLs, `git worktree add`,
//! `bd create` ids and written artifacts. Runs after every Bash/Write call,
//! so it is silent, never fails, never blocks and never creates the database.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::cli::LinkKind;
use crate::commands::link;
use crate::db::Db;
use crate::model::{Link, SessionStatus};

const DB_BUSY_MS: u32 = 2000;
const MAX_CAPTURES: usize = 20;
const MAX_BEADS: usize = 10;
const MAX_OUTPUT_TASKS: usize = 3;
const WRITE_NOTE: &str = "auto-captured (Write)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub kind: LinkKind,
    pub r#ref: String,
    pub meta: Option<Value>,
}

impl Capture {
    fn new(kind: LinkKind, r#ref: impl Into<String>) -> Capture {
        Capture {
            kind,
            r#ref: r#ref.into(),
            meta: None,
        }
    }
}

pub fn run() -> anyhow::Result<u8> {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    capture(&input);
    Ok(0)
}

/// Best effort end to end: any missing piece means nothing is recorded.
fn capture(input: &[u8]) {
    let Some(quest_ref) = env_var("Q_QUEST") else {
        return;
    };
    let Ok(payload) = serde_json::from_slice::<Value>(input) else {
        return;
    };
    let Some(tool) = payload["tool_name"].as_str() else {
        return;
    };
    let cwd = payload["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let Some(cwd) = cwd else {
        return;
    };
    let captures = extract(
        tool,
        &payload["tool_input"],
        &payload["tool_response"],
        &cwd,
    );
    if captures.is_empty() {
        return;
    }

    let Ok(path) = Db::path() else {
        return;
    };
    if !path.exists() {
        return;
    }
    let Ok(db) = Db::open_with_timeout(&path, DB_BUSY_MS) else {
        return;
    };
    let Ok(quest) = db.resolve_quest(&quest_ref) else {
        return;
    };
    let Some(session_id) = resolve_session(&db, &quest.id) else {
        return;
    };

    for c in captures {
        let (event_kind, note) = match c.kind {
            LinkKind::Artifact => ("artifact.added", Some(WRITE_NOTE)),
            _ => ("link.added", None),
        };
        // The same ref already linked under any kind counts as present.
        match db.find_link_by_ref(&quest.id, &c.r#ref) {
            Ok(None) => {}
            _ => continue,
        }
        let mut l = Link::new(&quest.id, c.kind.as_str(), &c.r#ref);
        l.session_id = session_id.clone();
        l.meta = c.meta;
        let _ = link::store(&db, l, event_kind, note, true);
    }
}

/// `Some(None)` only when no session env is present at all (quest-level
/// capture); once `$Q_SESSION` or `$TMUX_PANE` names a session it must be a
/// live one of this Quest, otherwise nothing is captured.
fn resolve_session(db: &Db, quest_id: &str) -> Option<Option<String>> {
    let session = match (env_var("Q_SESSION"), env_var("TMUX_PANE")) {
        (Some(id), _) => db.get_session(&id).ok()?,
        (None, Some(pane)) => db.find_session_by_pane(&pane).ok()?,
        (None, None) => return Some(None),
    };
    match session {
        Some(s) if s.quest_id == quest_id && s.status != SessionStatus::Ended => Some(Some(s.id)),
        _ => None,
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

// ------------------------------------------------------------------ extract

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r#"https?://(?:www\.)?(?:github\.com|app\.productive\.io)/[^\s"'<>\[\]()]+"#,
    )
}

/// Commands whose output is about one PR or task, so URLs printed by them
/// are worth linking; anything else (`gh pr list`, `git log`, `cat`) is not.
fn pr_cmd_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"\bgh\s+pr\s+(?:create|view|merge|checkout|status|edit|ready|comment|review)\b|\bgit\s+push\b",
    )
}

/// `&&`, `||`, `;` and newlines — the boundaries a `cd` reaches across.
fn segment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"&&|\|\||;|\n")
}

fn cd_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^\(*\s*cd\s+([^\s)]+)\s*$")
}

fn worktree_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"(?:^|[\s(])git\s+worktree\s+add\s+([^|\n]*)")
}

fn bd_cmd_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"(?:^|[\s;&|(])bd\s+(?:create|q|quick)(?:\s|$)")
}

fn bd_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"\bbd-[a-z0-9.]*[a-z0-9]")
}

/// Pure: everything worth linking in one PostToolUse payload, in order of
/// appearance, deduplicated, capped at `MAX_CAPTURES`.
pub fn extract(
    tool_name: &str,
    tool_input: &Value,
    tool_response: &Value,
    cwd: &Path,
) -> Vec<Capture> {
    let mut out: Vec<Capture> = Vec::new();
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    let mut push = |c: Capture| -> bool {
        let fresh = out.len() < MAX_CAPTURES && seen.insert((c.kind.as_str(), c.r#ref.clone()));
        if fresh {
            out.push(c);
        }
        fresh
    };
    match tool_name {
        "Bash" => extract_bash(tool_input, tool_response, cwd, &mut push),
        "Write" => {
            if let Some(c) = extract_write(tool_input, tool_response, cwd) {
                push(c);
            }
        }
        _ => {}
    }
    out
}

/// `tool_response` is `{stdout, stderr, interrupted}` today and was a plain
/// string in older Claude Code versions.
fn response_text(response: &Value) -> (String, String) {
    match response {
        Value::String(s) => (s.clone(), String::new()),
        Value::Object(o) => (
            o.get("stdout")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            o.get("stderr")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    }
}

fn extract_bash(
    input: &Value,
    response: &Value,
    cwd: &Path,
    push: &mut impl FnMut(Capture) -> bool,
) {
    let command = input["command"].as_str().unwrap_or("");
    let (stdout, stderr) = response_text(response);
    let interrupted = response["interrupted"].as_bool().unwrap_or(false);

    // Worktrees and beads first: they are rarer than URLs and must not be
    // starved by the cap. A worktree that failed to be added is not one.
    let failed = interrupted || stderr.contains("fatal:");
    if !failed {
        let mut base = cwd.to_path_buf();
        for segment in segment_re().split(command) {
            let segment = segment.trim();
            if let Some(m) = cd_re().captures(segment) {
                base = PathBuf::from(resolve(&base, m[1].trim_matches(['"', '\''])));
            } else if let Some(m) = worktree_re().captures(segment)
                && let Some((path, branch)) = parse_worktree_add(m[1].trim_end_matches(')'))
            {
                push(Capture::new(LinkKind::Worktree, resolve(&base, &path)));
                if let Some(b) = branch {
                    push(Capture::new(LinkKind::Branch, b));
                }
            }
        }
    }

    if bd_cmd_re().is_match(command) {
        for m in bd_id_re().find_iter(&stdout).take(MAX_BEADS) {
            push(Capture::new(LinkKind::Beads, m.as_str()));
        }
    }

    // URLs in the command are the agent's own intent; URLs in the output
    // only count when the command is about a PR/task (`gh pr list`, `git
    // log` and `cat CHANGELOG` print many PRs that are not this Quest's).
    let output_prs = pr_cmd_re().is_match(command);
    let output_tasks = output_prs || command.contains("productive");
    let mut tasks_from_output = 0;
    for (text, from_command) in [
        (command, true),
        (stdout.as_str(), false),
        (stderr.as_str(), false),
    ] {
        for m in url_re().find_iter(text) {
            let url = m.as_str().trim_end_matches(['.', ',', ':', ';']);
            let Some(rest) = link::strip_scheme(url) else {
                continue;
            };
            if link::is_github_pr(rest) {
                if from_command || output_prs {
                    push(Capture::new(LinkKind::Pr, link::normalize_pr_url(url)));
                }
            } else if link::is_productive_task(rest) {
                let canonical = link::normalize_productive_task_url(url);
                if from_command {
                    push(Capture::new(LinkKind::Task, canonical));
                } else if output_tasks
                    && tasks_from_output < MAX_OUTPUT_TASKS
                    && push(Capture::new(LinkKind::Task, canonical))
                {
                    tasks_from_output += 1;
                }
            }
        }
    }
}

/// The `<path>` and `-b/-B <branch>` of one `git worktree add` argument
/// list. A positional commit-ish is not a branch of this Quest (it is often
/// `main`, a SHA or `origin/x`), so only `-b/-B` yields one. Quotes are
/// stripped; other flags are skipped.
fn parse_worktree_add(args: &str) -> Option<(String, Option<String>)> {
    let mut path = None;
    let mut branch = None;
    let mut words = args.split_whitespace().map(|w| w.trim_matches(['"', '\'']));
    while let Some(w) = words.next() {
        match w {
            "-b" | "-B" => {
                branch = words.next().map(str::to_string);
            }
            "--reason" | "--orphan" => {
                words.next();
            }
            _ if w.starts_with('-') => {}
            _ if path.is_none() => path = Some(w.to_string()),
            _ => {}
        }
    }
    let path = path.filter(|p| !p.is_empty())?;
    Some((path, branch.filter(|b| !b.is_empty())))
}

/// `path` against `cwd` with `.`/`..` folded lexically (the target may not
/// exist yet), then canonicalised as far as `q link add` would.
fn resolve(cwd: &Path, path: &str) -> String {
    let joined = cwd.join(path);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    link::absolute(Path::new("/"), &out.to_string_lossy())
}

const IGNORED_DIRS: [&str; 3] = ["node_modules", ".git", "target"];

/// A file under `<cwd>/output/`, or a `*.md` directly in `<cwd>`.
fn extract_write(input: &Value, response: &Value, cwd: &Path) -> Option<Capture> {
    if response["success"].as_bool() == Some(false) {
        return None;
    }
    let file = input["file_path"].as_str().filter(|p| !p.is_empty())?;
    let abs = resolve(cwd, file);
    let base = resolve(cwd, ".");
    let rel = Path::new(&abs).strip_prefix(&base).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().unwrap_or(""),
            _ => "",
        })
        .collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || IGNORED_DIRS.contains(p))
    {
        return None;
    }
    let under_output = parts.len() >= 2 && parts[0] == "output";
    let md_in_root = parts.len() == 1 && parts[0].ends_with(".md");
    if !(under_output || md_in_root) {
        return None;
    }
    Some(Capture {
        kind: LinkKind::Artifact,
        r#ref: abs,
        meta: Some(serde_json::json!({ "note": WRITE_NOTE })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str, stdout: &str, stderr: &str) -> (Value, Value) {
        (
            json!({ "command": command }),
            json!({ "stdout": stdout, "stderr": stderr, "interrupted": false }),
        )
    }

    fn write(path: &str) -> (Value, Value) {
        (
            json!({ "file_path": path, "content": "x" }),
            json!({ "filePath": path, "success": true }),
        )
    }

    fn cap(kind: LinkKind, r: &str) -> Capture {
        Capture::new(kind, r)
    }

    #[test]
    fn extract_table() {
        use LinkKind as K;
        let cwd = Path::new("/nope/work");
        let pr = "https://github.com/acme/api/pull/42";
        type Case = (&'static str, &'static str, (Value, Value), Vec<Capture>);
        let cases: Vec<Case> = vec![
            (
                "pr url in stdout",
                "Bash",
                bash("gh pr create", &format!("{pr}\n"), ""),
                vec![cap(K::Pr, pr)],
            ),
            (
                "pr url twice, with suffixes, in gh pr view output → one",
                "Bash",
                bash(
                    "gh pr view 42",
                    &format!("url: {pr}/files\nsee {pr}?diff=split#r1."),
                    "",
                ),
                vec![cap(K::Pr, pr)],
            ),
            (
                "pr url in the command itself",
                "Bash",
                bash(&format!("open {pr}"), "", ""),
                vec![cap(K::Pr, pr)],
            ),
            (
                "pr url in stderr",
                "Bash",
                bash("gh pr merge", "", &format!("warning: {pr} has conflicts")),
                vec![cap(K::Pr, pr)],
            ),
            (
                "pr urls in gh pr list output → none",
                "Bash",
                bash(
                    "gh pr list",
                    &format!("{pr}\nhttps://github.com/acme/api/pull/43"),
                    "",
                ),
                vec![],
            ),
            (
                "pr urls in git log output → none",
                "Bash",
                bash("git log --oneline", &format!("abc merge {pr}"), ""),
                vec![],
            ),
            (
                "pr url in cat output → none",
                "Bash",
                bash("cat CHANGELOG.md", pr, ""),
                vec![],
            ),
            (
                "pr url in git push stderr → captured",
                "Bash",
                bash("git push -u origin feat/x", "", &format!("remote: {pr}")),
                vec![cap(K::Pr, pr)],
            ),
            (
                "github issue url → none",
                "Bash",
                bash("gh issue view", "https://github.com/acme/api/issues/42", ""),
                vec![],
            ),
            (
                "plain github repo url → none",
                "Bash",
                bash("git remote -v", "https://github.com/acme/api.git", ""),
                vec![],
            ),
            (
                "task deep link in command and tasks/<id> in output",
                "Bash",
                bash(
                    "curl 'https://app.productive.io/1-acme/tasks?filter=1&task/123'",
                    "https://app.productive.io/1-acme/tasks/456",
                    "",
                ),
                vec![
                    cap(K::Task, "https://app.productive.io/1-acme/tasks/123"),
                    cap(K::Task, "https://app.productive.io/1-acme/tasks/456"),
                ],
            ),
            (
                "task url in output of an unrelated command → none",
                "Bash",
                bash(
                    "cat notes.txt",
                    "https://app.productive.io/1-acme/tasks/456",
                    "",
                ),
                vec![],
            ),
            (
                "task urls from output are capped at three",
                "Bash",
                bash(
                    "curl productive-api",
                    "https://app.productive.io/1-acme/tasks/1 \
                     https://app.productive.io/1-acme/tasks/2 \
                     https://app.productive.io/1-acme/tasks/1 \
                     https://app.productive.io/1-acme/tasks/3 \
                     https://app.productive.io/1-acme/tasks/4",
                    "",
                ),
                vec![
                    cap(K::Task, "https://app.productive.io/1-acme/tasks/1"),
                    cap(K::Task, "https://app.productive.io/1-acme/tasks/2"),
                    cap(K::Task, "https://app.productive.io/1-acme/tasks/3"),
                ],
            ),
            (
                "productive non-task url → none",
                "Bash",
                bash("x", "https://app.productive.io/1-acme/projects/9", ""),
                vec![],
            ),
            (
                "worktree add with -b",
                "Bash",
                bash("git worktree add .worktrees/x -b feat/x", "", ""),
                vec![
                    cap(K::Worktree, "/nope/work/.worktrees/x"),
                    cap(K::Branch, "feat/x"),
                ],
            ),
            (
                "worktree add with positional commit-ish, absolute path, chained → no branch",
                "Bash",
                bash("cd /r && git worktree add /r/.wt/y main && ls", "", ""),
                vec![cap(K::Worktree, "/r/.wt/y")],
            ),
            (
                "worktree add after cd resolves against the new directory",
                "Bash",
                bash("cd repos/api && git worktree add ../wt/y", "", ""),
                vec![cap(K::Worktree, "/nope/work/repos/wt/y")],
            ),
            (
                "worktree add after cd in a subshell",
                "Bash",
                bash("(cd /r/api; git worktree add ../w -b f) && ls", "", ""),
                vec![cap(K::Worktree, "/r/w"), cap(K::Branch, "f")],
            ),
            (
                "worktree add before a cd is unaffected",
                "Bash",
                bash("git worktree add .wt/x && cd .wt/x", "", ""),
                vec![cap(K::Worktree, "/nope/work/.wt/x")],
            ),
            (
                "worktree and beads come before urls",
                "Bash",
                bash(
                    "git worktree add .wt/a -b x && gh pr create",
                    &format!("{pr}\n"),
                    "",
                ),
                vec![
                    cap(K::Worktree, "/nope/work/.wt/a"),
                    cap(K::Branch, "x"),
                    cap(K::Pr, pr),
                ],
            ),
            (
                "worktree add path only",
                "Bash",
                bash("git worktree add ../z", "Preparing worktree", ""),
                vec![cap(K::Worktree, "/nope/z")],
            ),
            (
                "worktree add failing with fatal → none",
                "Bash",
                bash(
                    "git worktree add .wt/x -b feat/x",
                    "",
                    "fatal: 'feat/x' is already checked out",
                ),
                vec![],
            ),
            (
                "worktree list → none",
                "Bash",
                bash("git worktree list", "/r  abc [main]", ""),
                vec![],
            ),
            (
                "worktree remove → none",
                "Bash",
                bash("git worktree remove .wt/x", "", ""),
                vec![],
            ),
            (
                "bd create with id in stdout",
                "Bash",
                bash("bd create 'title' -l repo:x", "Created bd-8lz.2.6\n", ""),
                vec![cap(K::Beads, "bd-8lz.2.6")],
            ),
            (
                "bd q with several ids, deduped",
                "Bash",
                bash("bd q \"a\"", "bd-a1 bd-a2 bd-a1", ""),
                vec![cap(K::Beads, "bd-a1"), cap(K::Beads, "bd-a2")],
            ),
            (
                "bd id in stdout of a non-create command → none",
                "Bash",
                bash("bd ready", "bd-a1 open", ""),
                vec![],
            ),
            (
                "bd create with the id only in the command → none",
                "Bash",
                bash("bd create --parent bd-a1 x", "ok", ""),
                vec![],
            ),
            (
                "interrupted command keeps urls but drops the worktree",
                "Bash",
                (
                    json!({ "command": format!("git worktree add w && echo {pr}") }),
                    json!({ "stdout": pr, "interrupted": true }),
                ),
                vec![cap(K::Pr, pr)],
            ),
            (
                "string tool_response (older payloads)",
                "Bash",
                (json!({ "command": "gh pr view" }), json!(pr)),
                vec![cap(K::Pr, pr)],
            ),
            (
                "write of source file → none",
                "Write",
                write("/nope/work/src/x.rs"),
                vec![],
            ),
            (
                "write under output/ → artifact",
                "Write",
                write("/nope/work/output/report.html"),
                vec![Capture {
                    kind: K::Artifact,
                    r#ref: "/nope/work/output/report.html".into(),
                    meta: Some(json!({ "note": WRITE_NOTE })),
                }],
            ),
            (
                "write of relative output/nested/x.md → artifact",
                "Write",
                write("output/nested/x.md"),
                vec![Capture {
                    kind: K::Artifact,
                    r#ref: "/nope/work/output/nested/x.md".into(),
                    meta: Some(json!({ "note": WRITE_NOTE })),
                }],
            ),
            (
                "write of notes.md in cwd → artifact",
                "Write",
                write("notes.md"),
                vec![Capture {
                    kind: K::Artifact,
                    r#ref: "/nope/work/notes.md".into(),
                    meta: Some(json!({ "note": WRITE_NOTE })),
                }],
            ),
            (
                "write of docs/README.md → none",
                "Write",
                write("/nope/work/docs/README.md"),
                vec![],
            ),
            (
                "write outside cwd → none",
                "Write",
                write("/elsewhere/output/x.md"),
                vec![],
            ),
            (
                "write under node_modules/output → none",
                "Write",
                write("node_modules/output/x.md"),
                vec![],
            ),
            (
                "failed write → none",
                "Write",
                (
                    json!({ "file_path": "notes.md" }),
                    json!({ "success": false }),
                ),
                vec![],
            ),
            ("other tool → none", "Read", write("notes.md"), vec![]),
        ];
        for (name, tool, (input, response), want) in cases {
            let got = extract(tool, &input, &response, cwd);
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn captures_are_capped() {
        let ids: Vec<String> = (0..30).map(|i| format!("bd-x{i}")).collect();
        let (i, r) = bash("bd create x", &ids.join(" "), "");
        assert_eq!(extract("Bash", &i, &r, Path::new("/w")).len(), MAX_BEADS);

        let urls: Vec<String> = (0..30)
            .map(|i| format!("https://github.com/a/b/pull/{i}"))
            .collect();
        let (i, r) = bash("gh pr view", &urls.join("\n"), "");
        assert_eq!(extract("Bash", &i, &r, Path::new("/w")).len(), MAX_CAPTURES);
    }

    #[test]
    fn worktree_args_are_parsed_leniently() {
        assert_eq!(
            parse_worktree_add("--detach \"a\" -B 'x'"),
            Some(("a".into(), Some("x".into())))
        );
        assert_eq!(parse_worktree_add("-f p"), Some(("p".into(), None)));
        assert_eq!(
            parse_worktree_add("p origin/main"),
            Some(("p".into(), None))
        );
        assert_eq!(parse_worktree_add("--lock"), None);
        assert_eq!(parse_worktree_add(""), None);
    }
}
