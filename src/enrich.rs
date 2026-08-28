//! Link enrichment (SPEC §12): lazily fill a link's `title` and `meta` with
//! real data — a PR's state and CI, a task's status and assignee, a worktree's
//! dirty count and tracking — cached on `enriched_at` with a 5-minute TTL.
//!
//! Enrichment is **best effort and never blocks the listing**. Every external
//! call goes through [`proc`] with a 3-second cap, a failure keeps the last
//! cached row rather than erroring, and stale links older than the TTL (or all
//! of them under `--refresh`) are re-fetched in a thread pool — no async
//! runtime, matching the rest of `q` (SPEC §21).
//!
//! The impure process layer is a [`Fetcher`] trait behind `$Q_FIXTURE` (the
//! same convention `beads.rs`/`tmux.rs` use): the real client shells out to
//! `gh`, `curl` and `git`, and every mapper that turns raw stdout into an
//! [`Enrichment`] is a pure function unit-tested on fixture strings.
//!
//! | var | stands for |
//! |---|---|
//! | `Q_FIXTURE_GH_PR` | stdout of `gh pr view … --json …` |
//! | `Q_FIXTURE_PRODUCTIVE` | body of `GET /api/v2/tasks/<id>` |
//! | `Q_FIXTURE_GIT_STATUS` | stdout of `git status --porcelain=v1 -b` |

use std::time::Duration;

use serde_json::{Map, Value};

use crate::db::Db;
use crate::model::{Link, now};
use crate::proc;

/// A cached reading counts as fresh for this long (SPEC §12: 5 minutes).
const ENRICH_TTL: i64 = 5 * 60;
/// Per-fetch cap. Enrichment must never hold up `q links` (SPEC §12: 3 s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// What a fetch produced: a display title and the `meta` keys the brief and
/// the listing render (`state`, `status`, `ci`, plus `assignee` for tasks).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Enrichment {
    pub title: Option<String>,
    pub meta: Map<String, Value>,
}

impl Enrichment {
    /// Nothing to store: neither a title nor a single meta key came back, so
    /// the caller keeps whatever the row already had.
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.meta.is_empty()
    }
}

// ------------------------------------------------------------------ the trait

/// Every external call enrichment makes. Stubbed under `$Q_FIXTURE`; `Sync`
/// because the fetches run together in a [`std::thread::scope`].
pub trait Fetcher: Sync {
    /// `gh pr view <ref> --json title,state,isDraft,reviewDecision,statusCheckRollup`.
    fn pr(&self, reference: &str) -> Option<String>;
    /// `GET https://api.productive.io/api/v2/tasks/<id>` — `None` when the
    /// token/org env is absent, so enrichment degrades to the bare ref.
    fn task(&self, id: &str) -> Option<String>;
    /// `git -C <path> status --porcelain=v1 -b`.
    fn worktree(&self, path: &str) -> Option<String>;
}

/// The real client, or the fixture one under `$Q_FIXTURE`.
pub fn client() -> Box<dyn Fetcher> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureFetcher),
        _ => Box::new(RealFetcher),
    }
}

struct RealFetcher;

impl Fetcher for RealFetcher {
    fn pr(&self, reference: &str) -> Option<String> {
        proc::run_capped(
            "gh",
            &[
                "pr",
                "view",
                reference,
                "--json",
                "title,state,isDraft,reviewDecision,statusCheckRollup",
            ],
            FETCH_TIMEOUT,
        )
    }

    fn task(&self, id: &str) -> Option<String> {
        let token = env_nonempty("PRODUCTIVE_API_TOKEN")?;
        let org = env_nonempty("PRODUCTIVE_ORG_ID")?;
        let url = format!("https://api.productive.io/api/v2/tasks/{id}");
        let auth = format!("X-Auth-Token: {token}");
        let orgh = format!("X-Organization-Id: {org}");
        proc::run_capped(
            "curl",
            &[
                "-sS",
                "--max-time",
                "3",
                "-H",
                auth.as_str(),
                "-H",
                orgh.as_str(),
                "-H",
                "Content-Type: application/vnd.api+json",
                url.as_str(),
            ],
            FETCH_TIMEOUT,
        )
    }

    fn worktree(&self, path: &str) -> Option<String> {
        proc::run_capped(
            "git",
            &["-C", path, "status", "--porcelain=v1", "-b"],
            FETCH_TIMEOUT,
        )
    }
}

struct FixtureFetcher;

impl Fetcher for FixtureFetcher {
    fn pr(&self, _: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_GH_PR")
    }
    fn task(&self, _: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_PRODUCTIVE")
    }
    fn worktree(&self, _: &str) -> Option<String> {
        fixture_file("Q_FIXTURE_GIT_STATUS")
    }
}

fn fixture_file(var: &str) -> Option<String> {
    std::fs::read_to_string(std::env::var_os(var)?).ok()
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ------------------------------------------------------------- orchestration

/// Enriches `links` in place, best effort: fetches every stale (or, under
/// `refresh`, every) PR/task/worktree link in a thread pool, writes the result
/// back through `db`, and leaves anything that failed on its prior cached row.
/// Never errors — a broken `gh`/`curl`/`git`, a missing token, or a timeout is
/// simply a link that keeps what it had.
pub fn enrich(db: &Db, links: &mut [Link], refresh: bool) {
    enrich_with(client().as_ref(), db, links, refresh);
}

fn enrich_with(fetcher: &dyn Fetcher, db: &Db, links: &mut [Link], refresh: bool) {
    let jobs: Vec<(usize, &Link)> = links
        .iter()
        .enumerate()
        .filter(|(_, l)| needs_fetch(l, refresh))
        .collect();
    if jobs.is_empty() {
        return;
    }
    let results = scatter(fetcher, jobs);
    let ts = now();
    for (idx, enrichment) in results {
        let link = &mut links[idx];
        // A fetch that came back with a title keeps it; one that came back with
        // only meta (a worktree always has a branch, but be safe) keeps the old.
        if enrichment.title.is_some() {
            link.title = enrichment.title;
        }
        link.meta = Some(Value::Object(enrichment.meta));
        link.enriched_at = Some(ts);
        let _ = db.update_enrichment(link.id, link.title.as_deref(), link.meta.as_ref(), ts);
    }
}

/// One thread per job, all joined before this returns; a job that fetched
/// nothing (or whose thread panicked) is simply dropped, so a single bad link
/// never takes the listing down.
fn scatter(fetcher: &dyn Fetcher, jobs: Vec<(usize, &Link)>) -> Vec<(usize, Enrichment)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(idx, link)| scope.spawn(move || fetch_one(fetcher, link).map(|e| (idx, e))))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    })
}

/// Whether this link is due for a fetch: only the enrichable kinds, and only
/// when `--refresh` forces it or the cache has gone stale.
fn needs_fetch(link: &Link, refresh: bool) -> bool {
    is_enrichable(&link.kind) && (refresh || is_stale(link.enriched_at))
}

fn is_enrichable(kind: &str) -> bool {
    matches!(kind, "pr" | "task" | "worktree")
}

fn is_stale(enriched_at: Option<i64>) -> bool {
    match enriched_at {
        None => true,
        Some(t) => (now() - t).abs() >= ENRICH_TTL,
    }
}

/// Shells out for one link and maps the raw stdout; `None` on any failure so
/// the caller keeps the cached row.
fn fetch_one(fetcher: &dyn Fetcher, link: &Link) -> Option<Enrichment> {
    let enrichment = match link.kind.as_str() {
        "pr" => map_pr(&fetcher.pr(&link.r#ref)?)?,
        "task" => {
            let id = task_id(&link.r#ref)?;
            map_task(&fetcher.task(id)?)?
        }
        "worktree" => map_worktree(&fetcher.worktree(&link.r#ref)?)?,
        _ => return None,
    };
    (!enrichment.is_empty()).then_some(enrichment)
}

// -------------------------------------------------------------- pure mappers

/// `gh pr view --json …` → title + `meta{state, status(review), ci}`. `state`
/// is `open`/`merged`/`closed`, or `draft` for an open draft; `status` is the
/// review decision; `ci` is the rolled-up check state.
pub fn map_pr(raw: &str) -> Option<Enrichment> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let mut meta = Map::new();

    let draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    if let Some(state) = v.get("state").and_then(Value::as_str) {
        let s = if draft && state.eq_ignore_ascii_case("open") {
            "draft".to_string()
        } else {
            state.to_lowercase()
        };
        meta.insert("state".into(), Value::String(s));
    }
    if let Some(rd) = v
        .get("reviewDecision")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        meta.insert("status".into(), Value::String(review_label(rd)));
    }
    if let Some(ci) = ci_rollup(v.get("statusCheckRollup")) {
        meta.insert("ci".into(), Value::String(ci));
    }

    Some(Enrichment {
        title: str_field(v.get("title")),
        meta,
    })
}

fn review_label(decision: &str) -> String {
    match decision.to_uppercase().as_str() {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" => "changes requested",
        "REVIEW_REQUIRED" => "review required",
        _ => return decision.to_lowercase(),
    }
    .to_string()
}

/// `statusCheckRollup` (a mix of `CheckRun`s with `status`/`conclusion` and
/// `StatusContext`s with `state`) → `failing` if anything failed, else
/// `pending` if anything is unfinished, else `passing`. An empty rollup means
/// no CI at all — `None`, so no `ci` key.
fn ci_rollup(v: Option<&Value>) -> Option<String> {
    let checks = v?.as_array()?;
    if checks.is_empty() {
        return None;
    }
    let mut failing = false;
    let mut pending = false;
    for c in checks {
        if let Some(state) = c.get("state").and_then(Value::as_str) {
            match state.to_uppercase().as_str() {
                "SUCCESS" => {}
                "PENDING" | "EXPECTED" => pending = true,
                _ => failing = true,
            }
            continue;
        }
        let done = c
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("completed"));
        if !done {
            pending = true;
            continue;
        }
        match c
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase()
            .as_str()
        {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "" => pending = true,
            _ => failing = true,
        }
    }
    Some(
        if failing {
            "failing"
        } else if pending {
            "pending"
        } else {
            "passing"
        }
        .to_string(),
    )
}

/// Productive `GET /tasks/<id>` (JSON:API) → title + `meta{status, assignee}`.
/// Defensive about the shape: a missing field is simply left out.
pub fn map_task(raw: &str) -> Option<Enrichment> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let data = v.get("data")?;
    let attrs = data.get("attributes");
    let mut meta = Map::new();

    if let Some(label) = attrs
        .and_then(|a| a.get("status"))
        .and_then(task_status_label)
    {
        meta.insert("status".into(), Value::String(label));
    }
    if let Some(name) = task_assignee(data, v.get("included")) {
        meta.insert("assignee".into(), Value::String(name));
    }

    Some(Enrichment {
        title: str_field(attrs.and_then(|a| a.get("title"))),
        meta,
    })
}

/// Productive stores task status as an integer (`1` open, `2` closed); a string
/// is passed through, anything else stringified.
fn task_status_label(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    match v.as_i64()? {
        1 => Some("open".to_string()),
        2 => Some("closed".to_string()),
        n => Some(n.to_string()),
    }
}

/// The assignee's name, resolved from the `included` people by the id in
/// `relationships.assignee`; falls back to that id when the person was not
/// side-loaded, and `None` when there is no assignee at all.
fn task_assignee(data: &Value, included: Option<&Value>) -> Option<String> {
    let id = data
        .get("relationships")?
        .get("assignee")?
        .get("data")?
        .get("id")?
        .as_str()?;
    let person = included.and_then(Value::as_array).and_then(|arr| {
        arr.iter().find(|it| {
            it.get("id").and_then(Value::as_str) == Some(id)
                && it
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "people" || t == "person")
        })
    });
    let attrs = person.and_then(|p| p.get("attributes"));
    let name = attrs.and_then(person_name);
    Some(name.unwrap_or_else(|| id.to_string()))
}

fn person_name(attrs: &Value) -> Option<String> {
    if let Some(name) = attrs
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    {
        return Some(name.to_string());
    }
    let first = attrs
        .get("first_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let last = attrs.get("last_name").and_then(Value::as_str).unwrap_or("");
    let full = format!("{first} {last}");
    let full = full.trim();
    (!full.is_empty()).then(|| full.to_string())
}

/// `git status --porcelain=v1 -b` → `meta{state}` like `3 dirty, ↑1 ↓0 on main`
/// and a title of the branch. The first line is the `## branch...upstream
/// [ahead/behind]` header; the rest are working-tree changes.
pub fn map_worktree(raw: &str) -> Option<Enrichment> {
    let header = raw.lines().next()?.strip_prefix("## ")?;
    let (branch, tracking) = parse_branch(header);
    let dirty = raw.lines().skip(1).filter(|l| !l.trim().is_empty()).count();

    let dirty_str = if dirty == 0 {
        "clean".to_string()
    } else {
        format!("{dirty} dirty")
    };
    let state = match tracking {
        Some((ahead, behind)) => format!("{dirty_str}, ↑{ahead} ↓{behind} on {branch}"),
        None => format!("{dirty_str} on {branch}"),
    };

    let mut meta = Map::new();
    meta.insert("state".into(), Value::String(state));
    Some(Enrichment {
        title: Some(branch),
        meta,
    })
}

/// Parses the porcelain branch header: `main...origin/main [ahead 1, behind 2]`
/// → (`main`, `Some((1, 2))`); a branch with no upstream → `None` tracking; a
/// detached HEAD → (`detached`, `None`).
fn parse_branch(header: &str) -> (String, Option<(i64, i64)>) {
    if header.starts_with("HEAD (no branch)") || header.starts_with("HEAD (") {
        return ("detached".to_string(), None);
    }
    let (names, bracket) = match header.split_once(" [") {
        Some((n, b)) => (n, Some(b.trim_end_matches(']'))),
        None => (header, None),
    };
    let branch = names
        .split("...")
        .next()
        .unwrap_or(names)
        .trim()
        .to_string();
    let has_upstream = names.contains("...");

    let (mut ahead, mut behind) = (0i64, 0i64);
    if let Some(b) = bracket {
        for part in b.split(',') {
            let part = part.trim();
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
    let tracking = has_upstream.then_some((ahead, behind));
    (branch, tracking)
}

/// The task's id out of the canonical `https://app.productive.io/<org>/tasks/<id>`
/// (also the older `…/task/<id>` and `?…task/<id>` deep links).
fn task_id(reference: &str) -> Option<&str> {
    let rest = reference
        .strip_prefix("https://")
        .or_else(|| reference.strip_prefix("http://"))
        .unwrap_or(reference);
    let path = rest.strip_prefix("app.productive.io/").unwrap_or(rest);
    let mut segments = path.split(['/', '?', '&', '=']).peekable();
    while let Some(seg) = segments.next() {
        if (seg == "task" || seg == "tasks")
            && let Some(id) = segments.peek()
            && !id.is_empty()
            && id.chars().all(|c| c.is_ascii_digit())
        {
            return Some(id);
        }
    }
    None
}

fn str_field(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Quest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ------------------------------------------------------------- PR mapper

    fn state(e: &Enrichment, key: &str) -> Option<String> {
        e.meta.get(key).and_then(Value::as_str).map(str::to_string)
    }

    #[test]
    fn pr_open_with_passing_ci_and_approval() {
        let raw = r#"{
            "title": "Fix the backfill",
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
                {"__typename":"StatusContext","state":"SUCCESS"}
            ]
        }"#;
        let e = map_pr(raw).unwrap();
        assert_eq!(e.title.as_deref(), Some("Fix the backfill"));
        assert_eq!(state(&e, "state").as_deref(), Some("open"));
        assert_eq!(state(&e, "status").as_deref(), Some("approved"));
        assert_eq!(state(&e, "ci").as_deref(), Some("passing"));
    }

    #[test]
    fn pr_draft_shows_draft_state_and_no_ci_for_empty_rollup() {
        let raw = r#"{"title":"WIP","state":"OPEN","isDraft":true,
            "reviewDecision":"","statusCheckRollup":[]}"#;
        let e = map_pr(raw).unwrap();
        assert_eq!(state(&e, "state").as_deref(), Some("draft"));
        assert!(!e.meta.contains_key("ci"), "empty rollup means no ci key");
        assert!(!e.meta.contains_key("status"), "empty review is dropped");
    }

    #[test]
    fn pr_merged_state() {
        let raw = r#"{"title":"Done","state":"MERGED","isDraft":false}"#;
        let e = map_pr(raw).unwrap();
        assert_eq!(state(&e, "state").as_deref(), Some("merged"));
    }

    #[test]
    fn pr_failing_ci_beats_pending_and_change_request() {
        let raw = r#"{
            "title":"Broken","state":"OPEN","isDraft":false,
            "reviewDecision":"CHANGES_REQUESTED",
            "statusCheckRollup":[
                {"status":"COMPLETED","conclusion":"FAILURE"},
                {"status":"IN_PROGRESS"},
                {"status":"COMPLETED","conclusion":"SUCCESS"}
            ]
        }"#;
        let e = map_pr(raw).unwrap();
        assert_eq!(state(&e, "ci").as_deref(), Some("failing"));
        assert_eq!(state(&e, "status").as_deref(), Some("changes requested"));
    }

    #[test]
    fn pr_pending_ci_when_a_check_is_still_running() {
        let raw = r#"{"title":"x","state":"OPEN","isDraft":false,
            "statusCheckRollup":[
                {"status":"COMPLETED","conclusion":"SUCCESS"},
                {"status":"QUEUED"}
            ]}"#;
        let e = map_pr(raw).unwrap();
        assert_eq!(state(&e, "ci").as_deref(), Some("pending"));
    }

    #[test]
    fn pr_garbage_is_none() {
        assert!(map_pr("not json").is_none());
    }

    // ----------------------------------------------------------- task mapper

    #[test]
    fn task_title_status_and_resolved_assignee() {
        let raw = r#"{
            "data": {
                "id": "123",
                "type": "tasks",
                "attributes": {"title": "Backfill CDC", "status": 1},
                "relationships": {"assignee": {"data": {"type": "people", "id": "77"}}}
            },
            "included": [
                {"id":"77","type":"people","attributes":{"name":"Ada Lovelace"}}
            ]
        }"#;
        let e = map_task(raw).unwrap();
        assert_eq!(e.title.as_deref(), Some("Backfill CDC"));
        assert_eq!(state(&e, "status").as_deref(), Some("open"));
        assert_eq!(state(&e, "assignee").as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn task_closed_status_and_first_last_name_assignee() {
        let raw = r#"{
            "data": {
                "attributes": {"title": "Old", "status": 2},
                "relationships": {"assignee": {"data": {"type": "people", "id": "9"}}}
            },
            "included": [
                {"id":"9","type":"people","attributes":{"first_name":"Grace","last_name":"Hopper"}}
            ]
        }"#;
        let e = map_task(raw).unwrap();
        assert_eq!(state(&e, "status").as_deref(), Some("closed"));
        assert_eq!(state(&e, "assignee").as_deref(), Some("Grace Hopper"));
    }

    #[test]
    fn task_unassigned_and_unsideloaded_assignee() {
        let unassigned = r#"{"data":{"attributes":{"title":"T","status":1},
            "relationships":{"assignee":{"data":null}}}}"#;
        let e = map_task(unassigned).unwrap();
        assert!(!e.meta.contains_key("assignee"));

        // Assignee present but not in `included`: fall back to the id.
        let bare = r#"{"data":{"attributes":{"title":"T","status":1},
            "relationships":{"assignee":{"data":{"type":"people","id":"42"}}}}}"#;
        let e = map_task(bare).unwrap();
        assert_eq!(state(&e, "assignee").as_deref(), Some("42"));
    }

    #[test]
    fn task_garbage_and_missing_data_are_none() {
        assert!(map_task("nope").is_none());
        assert!(map_task(r#"{"errors":[{"detail":"not found"}]}"#).is_none());
    }

    #[test]
    fn task_id_out_of_refs() {
        assert_eq!(
            task_id("https://app.productive.io/1-acme/tasks/98765"),
            Some("98765")
        );
        assert_eq!(
            task_id("https://app.productive.io/1-acme/task/9"),
            Some("9")
        );
        assert_eq!(
            task_id("https://app.productive.io/1-acme/tasks?filter=1&task/55"),
            Some("55")
        );
        assert_eq!(task_id("https://app.productive.io/1-acme/tasks"), None);
    }

    // -------------------------------------------------------- worktree mapper

    #[test]
    fn worktree_dirty_with_ahead_behind() {
        let raw = "## feat/x...origin/feat/x [ahead 1, behind 2]\n M src/a.rs\n?? b.txt\nA  c.rs\n";
        let e = map_worktree(raw).unwrap();
        assert_eq!(e.title.as_deref(), Some("feat/x"));
        assert_eq!(
            state(&e, "state").as_deref(),
            Some("3 dirty, ↑1 ↓2 on feat/x")
        );
    }

    #[test]
    fn worktree_clean_with_upstream() {
        let raw = "## main...origin/main\n";
        let e = map_worktree(raw).unwrap();
        assert_eq!(state(&e, "state").as_deref(), Some("clean, ↑0 ↓0 on main"));
    }

    #[test]
    fn worktree_ahead_only_and_no_upstream() {
        let ahead = "## main...origin/main [ahead 3]\n M x\n";
        assert_eq!(
            state(&map_worktree(ahead).unwrap(), "state").as_deref(),
            Some("1 dirty, ↑3 ↓0 on main")
        );
        let local = "## scratch\n M x\n";
        assert_eq!(
            state(&map_worktree(local).unwrap(), "state").as_deref(),
            Some("1 dirty on scratch")
        );
    }

    #[test]
    fn worktree_detached_head() {
        let raw = "## HEAD (no branch)\n M x\n";
        let e = map_worktree(raw).unwrap();
        assert_eq!(e.title.as_deref(), Some("detached"));
        assert_eq!(state(&e, "state").as_deref(), Some("1 dirty on detached"));
    }

    #[test]
    fn worktree_empty_input_is_none() {
        assert!(map_worktree("").is_none());
        assert!(map_worktree("not a header\n").is_none());
    }

    // --------------------------------------------------------- TTL / caching

    /// Records how many times each fetcher method was called and hands back a
    /// canned answer (or `None` to model a failure).
    struct StubFetcher {
        answer: Option<String>,
        calls: AtomicUsize,
    }

    impl StubFetcher {
        fn new(answer: Option<&str>) -> Self {
            StubFetcher {
                answer: answer.map(str::to_string),
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl Fetcher for StubFetcher {
        fn pr(&self, _: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.answer.clone()
        }
        fn task(&self, _: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.answer.clone()
        }
        fn worktree(&self, _: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.answer.clone()
        }
    }

    const PR_JSON: &str = r#"{"title":"Fresh","state":"OPEN","isDraft":false,
        "statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}"#;

    fn db_with_pr(enriched_at: Option<i64>) -> (Db, Quest, i64) {
        let db = Db::open_in_memory().unwrap();
        let q = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();
        let mut pr = Link::new(&q.id, "pr", "https://github.com/x/y/pull/1");
        pr.enriched_at = enriched_at;
        let id = db.insert_link(&pr).unwrap().id;
        (db, q, id)
    }

    #[test]
    fn fresh_link_is_not_fetched() {
        let (db, q, _) = db_with_pr(Some(now()));
        let mut links = db.list_links_by_quest(&q.id).unwrap();
        let stub = StubFetcher::new(Some(PR_JSON));
        enrich_with(&stub, &db, &mut links, false);
        assert_eq!(stub.calls(), 0, "a fresh cache must not fetch");
        assert!(links[0].meta.is_none());
    }

    #[test]
    fn stale_link_is_fetched_and_written_back() {
        let (db, q, id) = db_with_pr(Some(now() - ENRICH_TTL - 1));
        let mut links = db.list_links_by_quest(&q.id).unwrap();
        let stub = StubFetcher::new(Some(PR_JSON));
        enrich_with(&stub, &db, &mut links, false);
        assert_eq!(stub.calls(), 1);
        assert_eq!(links[0].title.as_deref(), Some("Fresh"));
        assert_eq!(
            links[0].meta.as_ref().unwrap()["ci"].as_str(),
            Some("passing")
        );
        // Persisted, not just in memory.
        let reloaded = db.get_link(id).unwrap().unwrap();
        assert_eq!(reloaded.title.as_deref(), Some("Fresh"));
        assert!(reloaded.enriched_at.is_some());
    }

    #[test]
    fn refresh_bypasses_a_fresh_cache() {
        let (db, q, _) = db_with_pr(Some(now()));
        let mut links = db.list_links_by_quest(&q.id).unwrap();
        let stub = StubFetcher::new(Some(PR_JSON));
        enrich_with(&stub, &db, &mut links, true);
        assert_eq!(stub.calls(), 1, "--refresh must ignore freshness");
    }

    #[test]
    fn a_failing_fetch_keeps_the_prior_cache() {
        let (db, q, id) = db_with_pr(Some(now() - ENRICH_TTL - 1));
        // Seed a prior enrichment so there is something to keep.
        db.update_enrichment(
            id,
            Some("Old title"),
            Some(&serde_json::json!({"state": "open"})),
            now() - ENRICH_TTL - 1,
        )
        .unwrap();
        let mut links = db.list_links_by_quest(&q.id).unwrap();
        let stub = StubFetcher::new(None); // fetch fails
        enrich_with(&stub, &db, &mut links, true);
        assert_eq!(stub.calls(), 1);
        let reloaded = db.get_link(id).unwrap().unwrap();
        assert_eq!(reloaded.title.as_deref(), Some("Old title"));
        assert_eq!(
            reloaded.meta.as_ref().unwrap()["state"].as_str(),
            Some("open")
        );
    }

    #[test]
    fn non_enrichable_kinds_are_left_alone() {
        let db = Db::open_in_memory().unwrap();
        let q = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();
        db.insert_link(&Link::new(&q.id, "url", "https://example.com"))
            .unwrap();
        db.insert_link(&Link::new(&q.id, "branch", "feat/x"))
            .unwrap();
        let mut links = db.list_links_by_quest(&q.id).unwrap();
        let stub = StubFetcher::new(Some(PR_JSON));
        enrich_with(&stub, &db, &mut links, true);
        assert_eq!(stub.calls(), 0);
    }
}
