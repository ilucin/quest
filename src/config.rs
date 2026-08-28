use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::cli::ConfigAction;
use crate::error::QError;
use crate::output;

/// `~/.config/q/config.toml`. Every level is `#[serde(default)]` so partial
/// files work, and `deny_unknown_fields` so typos are reported instead of
/// silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub machine: Machine,
    pub context: Context,
    pub naming: Naming,
    pub tmux: Tmux,
    pub statusline: Statusline,
    pub notify: Notify,
    pub ui: Ui,
    pub brain: Brain,
    pub beads: Beads,
    /// Array-of-tables; declared last so plain values never fall into it.
    /// toml hoists `remotes = []` above the preceding tables on its own, so
    /// the round-trip stays valid without skipping serialization.
    pub remotes: Vec<Remote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Machine {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub name: String,
    pub ssh: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Context {
    pub master_reset_pct: u8,
    pub worker_warn_pct: u8,
    pub reset_strategy: String,
    pub auto_reset: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Naming {
    pub auto: bool,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tmux {
    pub session_prefix: String,
    pub iterm_cc: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Statusline {
    /// Existing statusline command that `q` chains into.
    pub chain: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    pub macos: bool,
    pub ntfy_topic: String,
    pub on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    pub tick_local: u64,
    pub tick_remote: u64,
    pub rows: u8,
    pub mouse: bool,
    pub return_after_detach: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Brain {
    pub sync_links: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Beads {
    pub default_repo_label: String,
}

impl Default for Machine {
    fn default() -> Self {
        Machine {
            name: default_machine_name(),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Context {
            master_reset_pct: 35,
            worker_warn_pct: 70,
            reset_strategy: "clear".to_string(),
            auto_reset: true,
        }
    }
}

impl Default for Naming {
    fn default() -> Self {
        Naming {
            auto: true,
            model: "haiku".to_string(),
        }
    }
}

impl Default for Tmux {
    fn default() -> Self {
        Tmux {
            session_prefix: "q-".to_string(),
            iterm_cc: false,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Notify {
            macos: true,
            ntfy_topic: String::new(),
            on: vec![
                "waiting".to_string(),
                "reset".to_string(),
                "ended".to_string(),
            ],
        }
    }
}

impl Default for Ui {
    fn default() -> Self {
        Ui {
            tick_local: 2,
            tick_remote: 10,
            rows: 2,
            mouse: true,
            return_after_detach: true,
        }
    }
}

impl Default for Brain {
    fn default() -> Self {
        Brain { sync_links: true }
    }
}

impl Default for Beads {
    fn default() -> Self {
        Beads {
            default_repo_label: "global".to_string(),
        }
    }
}

pub const RESET_STRATEGIES: [&str; 2] = ["clear", "compact"];

impl Config {
    /// `$Q_CONFIG`, else `~/.config/q/config.toml`.
    pub fn path() -> anyhow::Result<PathBuf> {
        path_from(std::env::var_os("Q_CONFIG"))
    }

    pub fn load() -> anyhow::Result<Config> {
        Config::load_from(&Config::path()?)
    }

    /// A missing file is not an error — it yields the defaults, unwritten.
    pub fn load_from(path: &Path) -> anyhow::Result<Config> {
        let config = Config::parse_unchecked(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Like `load_from`, but does not validate — lets `config set` repair an
    /// invalid file by parsing it, applying the change, and validating only
    /// the result.
    fn parse_unchecked(path: &Path) -> anyhow::Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| QError::Config(format!("{}: {}", path.display(), tidy(&e))).into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(QError::Config(format!("{}: {e}", path.display())).into()),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_machine_name(&self.machine.name)?;

        for (key, pct) in [
            ("master_reset_pct", self.context.master_reset_pct),
            ("worker_warn_pct", self.context.worker_warn_pct),
        ] {
            if !(1..=100).contains(&pct) {
                return Err(bad(&format!(
                    "context.{key} must be between 1 and 100, got {pct}"
                )));
            }
        }

        if !RESET_STRATEGIES.contains(&self.context.reset_strategy.as_str()) {
            return Err(bad(&format!(
                "context.reset_strategy must be one of {}, got `{}`",
                RESET_STRATEGIES.join(" | "),
                self.context.reset_strategy
            )));
        }

        let mut seen: Vec<&str> = Vec::new();
        for remote in &self.remotes {
            if remote.name.is_empty() {
                return Err(bad("remotes[].name must not be empty"));
            }
            if remote.ssh.is_empty() {
                return Err(bad(&format!(
                    "remotes.{}.ssh must not be empty",
                    remote.name
                )));
            }
            if remote.name == self.machine.name {
                return Err(bad(&format!(
                    "remote `{}` reuses the local machine.name",
                    remote.name
                )));
            }
            if seen.contains(&remote.name.as_str()) {
                return Err(bad(&format!("duplicate remote `{}`", remote.name)));
            }
            seen.push(&remote.name);
        }

        if self.tmux.session_prefix.is_empty() {
            return Err(bad("tmux.session_prefix must not be empty"));
        }

        if self.ui.tick_local < 1 {
            return Err(bad(&format!(
                "ui.tick_local must be at least 1, got {}",
                self.ui.tick_local
            )));
        }
        if self.ui.tick_remote < 1 {
            return Err(bad(&format!(
                "ui.tick_remote must be at least 1, got {}",
                self.ui.tick_remote
            )));
        }
        if !(2..=3).contains(&self.ui.rows) {
            return Err(bad(&format!(
                "ui.rows must be between 2 and 3, got {}",
                self.ui.rows
            )));
        }

        Ok(())
    }

    fn to_toml_value(&self) -> anyhow::Result<toml::Value> {
        toml::Value::try_from(self).map_err(|e| QError::Config(e.to_string()).into())
    }

    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).map_err(|e| QError::Config(e.to_string()).into())
    }

    /// Reads a dotted key (`context.master_reset_pct`) off the effective config.
    pub fn get_key(&self, key: &str) -> anyhow::Result<toml::Value> {
        let root = self.to_toml_value()?;
        walk(&root, key)
            .cloned()
            .ok_or_else(|| QError::NotFound(format!("config key `{key}`")).into())
    }

    /// Sets a dotted key, coercing `raw` to the type the key already holds.
    pub fn set_key(&self, key: &str, raw: &str) -> anyhow::Result<Config> {
        let mut root = self.to_toml_value()?;
        let existing =
            walk(&root, key).ok_or_else(|| QError::NotFound(format!("config key `{key}`")))?;
        let coerced = coerce(existing, key, raw)?;

        let (parent_key, leaf) = key.rsplit_once('.').unwrap_or(("", key));
        let parent = if parent_key.is_empty() {
            &mut root
        } else {
            walk_mut(&mut root, parent_key).expect("parent resolved during lookup")
        };
        parent
            .as_table_mut()
            .expect("parent resolved during lookup")
            .insert(leaf.to_string(), coerced);

        let updated: Config = root
            .try_into()
            .map_err(|e: toml::de::Error| QError::Config(format!("{key}: {}", tidy(&e))))?;
        updated.validate()?;
        Ok(updated)
    }
}

fn path_from(env: Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(raw) = env
        && !raw.is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| QError::Config("cannot determine the home directory".to_string()))?;
    Ok(home.join(".config").join("q").join("config.toml"))
}

fn bad(msg: &str) -> anyhow::Error {
    QError::Config(msg.to_string()).into()
}

/// toml's multi-line errors read badly on a single `error:` line.
fn tidy(e: &impl std::fmt::Display) -> String {
    e.to_string()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shared by `Config::validate` (for `machine.name`) and `--machine`
/// (for the per-invocation override), which follows the same rules.
pub(crate) fn validate_machine_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        return Err(bad("machine.name must not be empty"));
    }
    if !is_machine_name(name) {
        return Err(bad(&format!(
            "machine.name `{name}` must match ^[a-z0-9][a-z0-9-]*$",
        )));
    }
    Ok(())
}

fn is_machine_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Hostname, reduced to the machine-name alphabet so the defaults validate.
fn default_machine_name() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let raw = std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default();
            normalize_machine_name(&raw)
        })
        .clone()
}

fn normalize_machine_name(raw: &str) -> String {
    let short = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mapped: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        "local".to_string()
    } else {
        trimmed.to_string()
    }
}

fn walk<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut cur = root;
    for seg in key.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = cur.as_table()?.get(seg)?;
    }
    Some(cur)
}

fn walk_mut<'a>(root: &'a mut toml::Value, key: &str) -> Option<&'a mut toml::Value> {
    let mut cur = root;
    for seg in key.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = cur.as_table_mut()?.get_mut(seg)?;
    }
    Some(cur)
}

fn coerce(existing: &toml::Value, key: &str, raw: &str) -> anyhow::Result<toml::Value> {
    let trimmed = raw.trim();
    match existing {
        toml::Value::String(_) => Ok(toml::Value::String(raw.to_string())),
        toml::Value::Integer(_) => trimmed
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| bad(&format!("`{key}` expects an integer, got `{raw}`"))),
        toml::Value::Float(_) => trimmed
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| bad(&format!("`{key}` expects a number, got `{raw}`"))),
        toml::Value::Boolean(_) => match trimmed {
            "true" => Ok(toml::Value::Boolean(true)),
            "false" => Ok(toml::Value::Boolean(false)),
            _ => Err(bad(&format!("`{key}` expects true or false, got `{raw}`"))),
        },
        other => Err(bad(&format!(
            "`{key}` holds {} data; edit it with `q config edit`",
            other.type_str()
        ))),
    }
}

/// Human rendering of a leaf value: bare scalars, TOML syntax for the rest.
fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => toml::to_string_pretty(other)
            .unwrap_or_else(|_| other.to_string())
            .trim_end()
            .to_string(),
    }
}

fn to_json(value: &toml::Value) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::to_value(value)?)
}

pub(crate) fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let io_err =
        |op: &str, e: std::io::Error| QError::Config(format!("{}: {op}: {e}", path.display()));

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| io_err("create directory", e))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, contents).map_err(|e| io_err("write", e))?;
    // Keep the original mode (e.g. 600) instead of the tmp file's umask default.
    if let Ok(meta) = fs::metadata(path) {
        fs::set_permissions(&tmp, meta.permissions()).map_err(|e| io_err("chmod", e))?;
    }
    fs::rename(&tmp, path).map_err(|e| io_err("rename", e))?;
    Ok(())
}

pub fn run(ctx: &Ctx, action: Option<&ConfigAction>) -> anyhow::Result<()> {
    match action {
        None => get(ctx, None),
        Some(ConfigAction::Get { key }) => get(ctx, key.as_deref()),
        Some(ConfigAction::Set { key, value }) => set(ctx, key, value),
        Some(ConfigAction::Edit) => edit(ctx),
        Some(ConfigAction::Path) => path_cmd(ctx),
    }
}

fn path_cmd(ctx: &Ctx) -> anyhow::Result<()> {
    let path = Config::path()?;
    let exists = path.exists();
    output::emit(
        ctx.json,
        &serde_json::json!({ "path": path, "exists": exists }),
        || path.display().to_string(),
    )
}

fn get(ctx: &Ctx, key: Option<&str>) -> anyhow::Result<()> {
    let Some(key) = key else {
        return output::emit(ctx.json, &ctx.config, || {
            ctx.config
                .to_toml_string()
                .map(|s| s.trim_end().to_string())
                .unwrap_or_else(|e| e.to_string())
        });
    };
    let value = ctx.config.get_key(key)?;
    let json = to_json(&value)?;
    output::emit(
        ctx.json,
        &serde_json::json!({ "key": key, "value": json }),
        || render(&value),
    )
}

/// `q config set` as a library call. Deliberately re-reads the file rather
/// than using `ctx.config`, so a `--machine` override for this invocation is
/// never persisted. Parsed without validating: an invalid file can still be
/// repaired by setting the one key that fixes it — only the result is
/// validated.
pub(crate) fn set_and_write(key: &str, raw: &str) -> anyhow::Result<Config> {
    let path = Config::path()?;
    let current = Config::parse_unchecked(&path)?;
    let updated = current.set_key(key, raw)?;
    write_atomic(&path, &updated.to_toml_string()?)?;
    Ok(updated)
}

fn set(ctx: &Ctx, key: &str, raw: &str) -> anyhow::Result<()> {
    let path = Config::path()?;
    let updated = set_and_write(key, raw)?;

    let value = updated.get_key(key)?;
    let json = to_json(&value)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "key": key, "value": json, "path": path }),
            || format!("{key} = {}", render(&value)),
        )?;
    }
    Ok(())
}

fn edit(ctx: &Ctx) -> anyhow::Result<()> {
    let path = Config::path()?;
    if !path.exists() {
        write_atomic(&path, &Config::default().to_toml_string()?)?;
    }

    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| bad("VISUAL/EDITOR is empty and `vi` is unavailable"))?;

    let status = std::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status()
        .map_err(|e| QError::Config(format!("cannot run editor `{editor}`: {e}")))?;
    if !status.success() {
        return Err(bad(&format!("editor `{editor}` exited with {status}")));
    }

    // Report problems; never rewrite what the user just edited.
    Config::load_from(&path)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "path": path, "valid": true }),
            || format!("{} is valid", path.display()),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(e: &anyhow::Error) -> &'static str {
        e.downcast_ref::<QError>()
            .map(QError::code)
            .unwrap_or("other")
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.context.master_reset_pct, 35);
        assert_eq!(c.context.worker_warn_pct, 70);
        assert_eq!(c.context.reset_strategy, "clear");
        assert!(c.context.auto_reset);
        assert!(c.naming.auto);
        assert_eq!(c.naming.model, "haiku");
        assert_eq!(c.tmux.session_prefix, "q-");
        assert!(!c.tmux.iterm_cc);
        assert_eq!(c.statusline.chain, "");
        assert!(c.notify.macos);
        assert_eq!(c.notify.on, ["waiting", "reset", "ended"]);
        assert_eq!(c.ui.tick_local, 2);
        assert_eq!(c.ui.tick_remote, 10);
        assert_eq!(c.ui.rows, 2);
        assert!(c.ui.mouse);
        assert!(c.ui.return_after_detach);
        assert!(c.brain.sync_links);
        assert_eq!(c.beads.default_repo_label, "global");
        assert!(c.remotes.is_empty());
        c.validate().unwrap();
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let c = Config::default();
        let text = c.to_toml_string().unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back, "round-trip mismatch for:\n{text}");
    }

    #[test]
    fn remotes_round_trip_through_toml() {
        let mut c = Config::default();
        c.remotes.push(Remote {
            name: "ws".to_string(),
            ssh: "ws".to_string(),
        });
        let text = c.to_toml_string().unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back, "round-trip mismatch for:\n{text}");
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let c: Config = toml::from_str("[context]\nmaster_reset_pct = 50\n").unwrap();
        assert_eq!(c.context.master_reset_pct, 50);
        assert_eq!(c.context.worker_warn_pct, 70);
        assert_eq!(c.tmux.session_prefix, "q-");
        assert_eq!(c.machine.name, Config::default().machine.name);
    }

    #[test]
    fn empty_file_is_the_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let e =
            toml::from_str::<Config>("[tmux]\nsession_prefix = \"q-\"\nnope = 1\n").unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        let e = toml::from_str::<Config>("[nope]\nx = 1\n").unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_reports_the_path_on_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        fs::write(&p, "[machine\n").unwrap();
        let e = Config::load_from(&p).unwrap_err();
        assert_eq!(code_of(&e), "config");
        assert!(e.to_string().contains("config.toml"), "{e}");
    }

    #[test]
    fn path_prefers_the_env_override() {
        let p = path_from(Some(OsString::from("/tmp/q-test/config.toml"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/q-test/config.toml"));
        let p = path_from(Some(OsString::new())).unwrap();
        assert!(p.ends_with(".config/q/config.toml"), "{}", p.display());
        let p = path_from(None).unwrap();
        assert!(p.ends_with(".config/q/config.toml"), "{}", p.display());
    }

    fn invalid(mutate: impl FnOnce(&mut Config)) -> String {
        let mut c = Config::default();
        mutate(&mut c);
        let e = c.validate().unwrap_err();
        assert_eq!(code_of(&e), "config");
        e.to_string()
    }

    #[test]
    fn validation_rules() {
        assert!(invalid(|c| c.machine.name = String::new()).contains("empty"));
        assert!(invalid(|c| c.machine.name = "Laptop".into()).contains("machine.name"));
        assert!(invalid(|c| c.machine.name = "-laptop".into()).contains("machine.name"));
        assert!(invalid(|c| c.machine.name = "lap_top".into()).contains("machine.name"));
        assert!(invalid(|c| c.context.master_reset_pct = 0).contains("master_reset_pct"));
        assert!(invalid(|c| c.context.worker_warn_pct = 0).contains("worker_warn_pct"));
        assert!(invalid(|c| c.context.reset_strategy = "nuke".into()).contains("reset_strategy"));
        assert!(invalid(|c| c.tmux.session_prefix = String::new()).contains("session_prefix"));
        assert!(
            invalid(|c| c.remotes = vec![
                Remote {
                    name: "ws".into(),
                    ssh: "ws".into()
                },
                Remote {
                    name: "ws".into(),
                    ssh: "ws2".into()
                },
            ])
            .contains("duplicate")
        );
        assert!(
            invalid(|c| c.remotes = vec![Remote {
                name: String::new(),
                ssh: "ws".into()
            }])
            .contains("name")
        );
        assert!(
            invalid(|c| {
                c.machine.name = "laptop".into();
                c.remotes = vec![Remote {
                    name: "laptop".into(),
                    ssh: "ws".into(),
                }];
            })
            .contains("local machine.name")
        );
        assert!(
            invalid(|c| c.remotes = vec![Remote {
                name: "ws".into(),
                ssh: String::new()
            }])
            .contains("ssh")
        );
        assert!(invalid(|c| c.ui.tick_local = 0).contains("tick_local"));
        assert!(invalid(|c| c.ui.tick_remote = 0).contains("tick_remote"));
        assert!(invalid(|c| c.ui.rows = 1).contains("rows"));
        assert!(invalid(|c| c.ui.rows = 4).contains("rows"));
        // 100 is the inclusive upper bound; 2 and 3 are both valid rows.
        let mut c = Config::default();
        c.context.master_reset_pct = 100;
        c.context.worker_warn_pct = 1;
        c.ui.rows = 3;
        c.validate().unwrap();
    }

    #[test]
    fn machine_name_normalization() {
        assert_eq!(
            normalize_machine_name("Ivans-MacBook-Pro.local"),
            "ivans-macbook-pro"
        );
        assert_eq!(normalize_machine_name("  ws  "), "ws");
        assert_eq!(normalize_machine_name("_odd_"), "odd");
        assert_eq!(normalize_machine_name(""), "local");
        assert!(is_machine_name(&default_machine_name()));
    }

    #[test]
    fn dotted_get() {
        let c = Config::default();
        assert_eq!(
            c.get_key("context.master_reset_pct").unwrap().as_integer(),
            Some(35)
        );
        assert_eq!(
            c.get_key("machine.name").unwrap().as_str(),
            Some(c.machine.name.as_str())
        );
        assert_eq!(c.get_key("ui.mouse").unwrap().as_bool(), Some(true));
        assert!(c.get_key("notify.on").unwrap().is_array());
        assert!(c.get_key("tmux").unwrap().is_table());
    }

    #[test]
    fn dotted_get_unknown_key_is_not_found() {
        let c = Config::default();
        for key in [
            "nope",
            "context.nope",
            "machine.name.deeper",
            "",
            "context.",
        ] {
            let e = c.get_key(key).unwrap_err();
            assert_eq!(code_of(&e), "not_found", "key `{key}`");
        }
    }

    #[test]
    fn dotted_set_coerces_from_the_existing_type() {
        let c = Config::default();
        assert_eq!(c.set_key("machine.name", "ws").unwrap().machine.name, "ws");
        assert_eq!(
            c.set_key("context.master_reset_pct", "42")
                .unwrap()
                .context
                .master_reset_pct,
            42
        );
        assert!(!c.set_key("ui.mouse", "false").unwrap().ui.mouse);
        assert_eq!(
            c.set_key("statusline.chain", "  x  ")
                .unwrap()
                .statusline
                .chain,
            "  x  "
        );
    }

    #[test]
    fn dotted_set_rejects_bad_values() {
        let c = Config::default();
        let cases = [
            ("context.master_reset_pct", "high", "integer"),
            ("ui.mouse", "yes", "true or false"),
            ("notify.on", "waiting", "array"),
            ("tmux", "x", "table"),
            ("context.master_reset_pct", "0", "between 1 and 100"),
            ("context.master_reset_pct", "999", ""),
            ("context.reset_strategy", "nuke", "reset_strategy"),
            ("machine.name", "Bad Name", "machine.name"),
        ];
        for (key, value, needle) in cases {
            let e = c.set_key(key, value).unwrap_err();
            assert_eq!(code_of(&e), "config", "{key}={value}: {e}");
            assert!(e.to_string().contains(needle), "{key}={value}: {e}");
        }
        let e = c.set_key("nope.nope", "1").unwrap_err();
        assert_eq!(code_of(&e), "not_found");
    }

    #[test]
    fn atomic_write_creates_parents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("config.toml");
        write_atomic(&p, "x = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "x = 1\n");
        let entries: Vec<_> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
    }
}
