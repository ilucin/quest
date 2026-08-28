//! Template placeholders and the TOML document `q tpl export`, `q tpl import`
//! and `q tpl edit` exchange (SPEC §11).
//!
//! Everything here is pure: the database is `db::template`'s and the command
//! plumbing is `commands::tpl`'s, so the two things a template file has to get
//! right — what a placeholder means, and what survives a round trip — are
//! unit-testable on their own.
//!
//! # The document
//!
//! One array of tables, whatever the file holds:
//!
//! ```toml
//! [[template]]
//! name = "weekly-hygiene"
//! goal = "tidy the work repo, {{date}}"
//! ```
//!
//! A single `q tpl export <name>` writes the same shape as `q tpl export`, so
//! one file format goes in and out and a one-template export imports again
//! without editing.
//!
//! A [`Definition`] carries the template's **definition** and nothing else.
//! `id`, `run_count`, `last_run_at`, `created_at` and `updated_at` never
//! travel: they are this machine's record of what happened, not part of what
//! the template *is*, and a file that carried them would let an import rewrite
//! history by hand. That is also why `q tpl import --replace` keeps the run
//! stats of the row it overwrites.
//!
//! Every field but `name` may be omitted, and an omitted field is exactly a
//! blank one — the render writes them all so `q tpl edit` hands the user a
//! complete skeleton rather than a form whose fields they have to remember.
//!
//! # Placeholders
//!
//! `goal` and `master_prompt` support `{{date}}` (today, local, `YYYY-MM-DD`)
//! and `{{arg.k}}` (from `q tpl run --arg k=v`). Anything else between double
//! braces, and any `arg.k` no `--arg` supplied, is an **error** naming every
//! offending key at once — see [`expand`]. A prompt is what an agent is about
//! to be told; shipping it with `{{arg.ticket}}` still in it is worse than not
//! running at all.
//!
//! # Escaping
//!
//! A prompt about templating carries `{{…}}` of its own — Mustache, Jinja,
//! Handlebars, a Claude skill — so **doubling escapes**: `{{{{` is a literal
//! `{{` and `}}}}` a literal `}}`, neither of which is a placeholder. It is
//! the `{{` → `{` rule of `format!`, one level up, and it is what
//! [`escape`] writes so `q tpl from` can capture a Quest whose text
//! `q new` accepted. A `{{` with no `}}` after it is still text, unchanged.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::QError;
use crate::model::Template;

/// The whole TOML document: `[[template]]`, repeated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Document {
    #[serde(default, rename = "template")]
    pub templates: Vec<Definition>,
}

/// One template as a file spells it. Blank is absent, for every field: a TOML
/// key set to `""` and a key that is not there at all both mean SQL NULL, so
/// the render can print every field without inventing content.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub master_prompt: String,
    #[serde(default)]
    pub beads_repo: String,
    #[serde(default)]
    pub create_brain: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Definition {
    /// The definition half of a stored row.
    pub fn of(t: &Template) -> Definition {
        Definition {
            name: t.name.clone(),
            description: t.description.clone().unwrap_or_default(),
            cwd: t.cwd.clone().unwrap_or_default(),
            workflow: t.workflow.clone().unwrap_or_default(),
            goal: t.goal.clone().unwrap_or_default(),
            master_prompt: t.master_prompt.clone().unwrap_or_default(),
            beads_repo: t.beads_repo.clone().unwrap_or_default(),
            create_brain: t.create_brain,
            tags: t.tags.clone().unwrap_or_default(),
        }
    }

    /// Writes this definition over `row`, leaving its identity and its run
    /// stats alone.
    pub fn apply(&self, row: &mut Template) {
        row.name = self.name.trim().to_string();
        row.description = blank_to_none(&self.description);
        row.cwd = blank_to_none(&self.cwd);
        row.workflow = blank_to_none(&self.workflow);
        row.goal = blank_to_none(&self.goal);
        row.master_prompt = blank_to_none(&self.master_prompt);
        row.beads_repo = blank_to_none(&self.beads_repo);
        row.create_brain = self.create_brain;
        row.tags = clean_tags(&self.tags);
    }
}

/// `None` for blank, so an empty TOML key and a missing one agree. Only the
/// ends are trimmed — a prompt keeps the shape the user gave it.
pub fn blank_to_none(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Blank tags dropped, order kept, duplicates removed; nothing left is NULL.
pub fn clean_tags(tags: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && !out.iter().any(|t| t == tag) {
            out.push(tag.to_string());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// One or many templates as the TOML `q tpl export` prints and `q tpl edit`
/// opens.
pub fn render(templates: &[Template]) -> anyhow::Result<String> {
    let doc = Document {
        templates: templates.iter().map(Definition::of).collect(),
    };
    toml::to_string_pretty(&doc)
        .map_err(|e| QError::Invalid(format!("cannot render TOML: {e}")).into())
}

/// The inverse of [`render`]. A file with no `[[template]]` at all parses to an
/// empty document; whether that is an error is the caller's (`q tpl edit` says
/// yes, an empty import says nothing happened).
pub fn parse(text: &str) -> anyhow::Result<Document> {
    toml::from_str(text).map_err(|e| QError::Invalid(format!("invalid TOML: {}", tidy(&e))).into())
}

/// Whether the document says `template` **at all**.
///
/// `template = []` and a file with no `template` key both [`parse`] to an
/// empty document, and they are not the same thing: the first is what
/// `q tpl export` writes for an empty database, so a scripted backup and
/// restore of one has to be a no-op; the second is not a template file.
pub fn declares_templates(text: &str) -> bool {
    text.parse::<toml::Table>()
        .is_ok_and(|table| table.contains_key("template"))
}

/// toml's errors carry a multi-line span; one line is what an error message
/// has room for.
fn tidy(e: &toml::de::Error) -> String {
    e.message()
        .lines()
        .next()
        .unwrap_or("parse error")
        .trim()
        .to_string()
}

/// Something the scanner found, and the byte range it spans.
enum Token {
    /// `{{name}}` — a placeholder to fill.
    Slot {
        name: String,
        start: usize,
        end: usize,
    },
    /// `{{{{` or `}}}}` — the escape for the literal it carries.
    Escaped {
        literal: &'static str,
        start: usize,
        end: usize,
    },
}

/// Every placeholder and every escape in `text`, in order. An unclosed `{{`
/// is neither — it is text that happens to contain two braces, and rewriting
/// it would be a worse surprise than leaving it alone.
fn scan(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        // A doubled brace pair is a literal one, and never opens a
        // placeholder: `{{{{arg.k}}}}` is text about `{{arg.k}}`.
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            if text[i..].starts_with("}}}}") {
                out.push(Token::Escaped {
                    literal: "}}",
                    start: i,
                    end: i + 4,
                });
                i += 4;
            } else {
                i += 2;
            }
            continue;
        }
        if bytes[i] != b'{' || bytes[i + 1] != b'{' {
            i += 1;
            continue;
        }
        if text[i..].starts_with("{{{{") {
            out.push(Token::Escaped {
                literal: "{{",
                start: i,
                end: i + 4,
            });
            i += 4;
            continue;
        }
        let Some(rel) = text[i + 2..].find("}}") else {
            break;
        };
        let inner_end = i + 2 + rel;
        out.push(Token::Slot {
            name: text[i + 2..inner_end].trim().to_string(),
            start: i,
            end: inner_end + 2,
        });
        i = inner_end + 2;
    }
    out
}

/// `text` with every `{{` and `}}` doubled, so [`expand`] gives it back
/// exactly — what `q tpl from` runs over what it copies out of a Quest, whose
/// goal and prompt were never placeholder syntax to begin with.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let doubled = i + 1 < bytes.len()
            && ((bytes[i] == b'{' && bytes[i + 1] == b'{')
                || (bytes[i] == b'}' && bytes[i + 1] == b'}'));
        if doubled {
            out.push_str(&text[i..i + 2]);
            out.push_str(&text[i..i + 2]);
            i += 2;
            continue;
        }
        let ch = text[i..].chars().next().expect("i is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Placeholders a template used that nothing can fill. Both lists are
/// deduplicated and kept in the order they appear, so the error message reads
/// like the template does.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unresolved {
    /// `{{arg.k}}` for a `k` no `--arg` supplied.
    pub missing: Vec<String>,
    /// `{{…}}` that is neither `date` nor a well-formed `arg.<key>`.
    pub unknown: Vec<String>,
}

impl Unresolved {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.unknown.is_empty()
    }

    fn push(list: &mut Vec<String>, value: &str) {
        if !list.iter().any(|v| v == value) {
            list.push(value.to_string());
        }
    }

    fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!(
                "no --arg for {}",
                self.missing
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.unknown.is_empty() {
            parts.push(format!(
                "unknown placeholder {}",
                self.unknown
                    .iter()
                    .map(|k| format!("`{{{{{k}}}}}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts
    }
}

/// One error for **every** field of one template that could not be filled.
///
/// A template is expanded as a whole, so its holes are reported as a whole:
/// discovering `master_prompt`'s missing key only after supplying `goal`'s
/// costs a second failed `q tpl run` for something the first one already knew.
pub fn unresolved_error(bad: Vec<(String, Unresolved)>) -> anyhow::Error {
    let fields = bad
        .iter()
        .filter(|(_, u)| !u.is_empty())
        .map(|(field, u)| format!("{field}: {}", u.parts().join("; ")))
        .collect::<Vec<_>>()
        .join("; ");
    QError::Invalid(format!(
        "{fields} (supported: {{{{date}}}}, {{{{arg.<key>}}}}; \
         `{{{{{{{{` is a literal `{{{{`)"
    ))
    .into()
}

/// An `arg.<key>` key: what `--arg k=v` accepts on the left of the `=`.
pub fn is_arg_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `{{date}}` and `{{arg.k}}` substituted; anything that cannot be filled is
/// collected rather than left in the text (see the module docs).
///
/// `date` is passed in rather than read from the clock so the caller expands
/// every field of one run against the same day — a `q tpl run` that straddles
/// midnight must not put two dates in one Quest.
pub fn expand(
    text: &str,
    date: &str,
    args: &BTreeMap<String, String>,
) -> Result<String, Unresolved> {
    let mut bad = Unresolved::default();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for token in scan(text) {
        let (name, start, end) = match token {
            Token::Escaped {
                literal,
                start,
                end,
            } => {
                out.push_str(&text[cursor..start]);
                out.push_str(literal);
                cursor = end;
                continue;
            }
            Token::Slot { name, start, end } => (name, start, end),
        };
        out.push_str(&text[cursor..start]);
        cursor = end;
        if name == "date" {
            out.push_str(date);
            continue;
        }
        match name.strip_prefix("arg.").filter(|k| is_arg_key(k)) {
            Some(key) => match args.get(key) {
                Some(value) => out.push_str(value),
                None => Unresolved::push(&mut bad.missing, key),
            },
            None => Unresolved::push(&mut bad.unknown, &name),
        }
    }
    out.push_str(&text[cursor..]);
    if bad.is_empty() { Ok(out) } else { Err(bad) }
}

/// The placeholders a text uses that can never be filled, whatever `--arg` is
/// given — a typo (`{{today}}`, `{{arg.}}`) that `q tpl add`/`edit` can refuse
/// at the point it is written instead of at the point it is run.
pub fn unknown_placeholders(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in scan(text) {
        let Token::Slot { name, .. } = token else {
            continue;
        };
        if name == "date" || name.strip_prefix("arg.").is_some_and(is_arg_key) {
            continue;
        }
        Unresolved::push(&mut out, &name);
    }
    out
}

/// The `arg.<key>` keys `text` actually uses — what tells `q tpl run` that an
/// `--arg` it was handed fills nothing.
pub fn arg_keys(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in scan(text) {
        let Token::Slot { name, .. } = token else {
            continue;
        };
        if let Some(key) = name.strip_prefix("arg.").filter(|k| is_arg_key(k)) {
            Unresolved::push(&mut out, key);
        }
    }
    out
}

/// `q tpl add`/`edit`'s gate: a stored field may only use placeholders that
/// something could fill. Every field is checked before anything is reported,
/// for the reason [`unresolved_error`] gives.
pub fn check_placeholders(fields: &[(&str, Option<&str>)]) -> anyhow::Result<()> {
    let bad: Vec<(String, Unresolved)> = fields
        .iter()
        .map(|(field, text)| {
            (
                (*field).to_string(),
                Unresolved {
                    missing: Vec::new(),
                    unknown: unknown_placeholders(text.unwrap_or_default()),
                },
            )
        })
        .filter(|(_, u)| !u.is_empty())
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(unresolved_error(bad))
}

/// Today, local, ISO — what `{{date}}` expands to.
pub fn today() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// `k=v`, as `--arg` is repeated. A key may be given once; a value may contain
/// `=` and may be empty.
pub fn parse_args(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| QError::Invalid(format!("--arg `{pair}`: expected k=v")))?;
        let key = key.trim();
        if !is_arg_key(key) {
            return Err(QError::Invalid(format!(
                "--arg `{pair}`: `{key}` is not a key (letters, digits, `_` and `-`)"
            ))
            .into());
        }
        if out.insert(key.to_string(), value.to_string()).is_some() {
            return Err(QError::Invalid(format!("--arg `{key}` was given twice")).into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn placeholders_are_substituted_in_place() {
        let a = args(&[("ticket", "PRD-1"), ("who", "ivan")]);
        assert_eq!(
            expand("{{date}}: {{arg.ticket}} for {{arg.who}}", "2026-08-28", &a).unwrap(),
            "2026-08-28: PRD-1 for ivan"
        );
        // Whitespace inside the braces is allowed; nothing else changes.
        assert_eq!(
            expand("a {{ date }} b", "2026-08-28", &a).unwrap(),
            "a 2026-08-28 b"
        );
        // An empty value substitutes to nothing rather than being "missing".
        assert_eq!(
            expand("[{{arg.x}}]", "2026-08-28", &args(&[("x", "")])).unwrap(),
            "[]"
        );
    }

    #[test]
    fn text_without_placeholders_is_returned_untouched() {
        let none = BTreeMap::new();
        for text in ["", "plain", "a { b } c", "an unclosed {{ tail", "}}{{"] {
            assert_eq!(expand(text, "2026-08-28", &none).unwrap(), text, "{text}");
        }
    }

    #[test]
    fn every_unfillable_placeholder_is_reported_at_once() {
        let bad = expand(
            "{{arg.a}} {{today}} {{arg.b}} {{arg.a}} {{arg.}}",
            "2026-08-28",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(bad.missing, ["a", "b"]);
        assert_eq!(bad.unknown, ["today", "arg."]);
        let msg = unresolved_error(vec![("goal".to_string(), bad)]).to_string();
        assert!(msg.contains("goal:"), "{msg}");
        assert!(msg.contains("`a`") && msg.contains("`b`"), "{msg}");
        assert!(msg.contains("`{{today}}`"), "{msg}");
    }

    #[test]
    fn a_placeholder_a_run_could_never_fill_is_caught_when_it_is_stored() {
        assert!(unknown_placeholders("{{date}} {{arg.x}}").is_empty());
        assert_eq!(unknown_placeholders("{{ARG.x}} {{date }}"), ["ARG.x"]);
        assert!(check_placeholders(&[("goal", Some("{{arg.x}}"))]).is_ok());
        assert!(check_placeholders(&[("goal", None)]).is_ok());
        let e = check_placeholders(&[("master_prompt", Some("{{nope}}"))]).unwrap_err();
        assert!(e.to_string().contains("master_prompt"), "{e}");
    }

    /// One error for the whole template, not one per field.
    #[test]
    fn every_field_that_cannot_be_filled_is_named_in_one_error() {
        let e = check_placeholders(&[
            ("goal", Some("{{nope}}")),
            ("master_prompt", Some("{{alsonope}}")),
        ])
        .unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("goal: unknown placeholder `{{nope}}`"),
            "{msg}"
        );
        assert!(msg.contains("master_prompt:"), "{msg}");
        assert!(msg.contains("alsonope"), "{msg}");

        let both = unresolved_error(vec![
            (
                "goal".to_string(),
                Unresolved {
                    missing: vec!["a".to_string()],
                    unknown: Vec::new(),
                },
            ),
            (
                "master_prompt".to_string(),
                Unresolved {
                    missing: vec!["b".to_string()],
                    unknown: Vec::new(),
                },
            ),
        ])
        .to_string();
        assert!(both.contains("goal: no --arg for `a`"), "{both}");
        assert!(both.contains("master_prompt: no --arg for `b`"), "{both}");
    }

    /// A prompt about templating is routine here, so `{{` has an escape.
    #[test]
    fn a_doubled_brace_is_a_literal_one_and_never_a_placeholder() {
        let none = BTreeMap::new();
        assert_eq!(
            expand("render {{{{user.name}}}} please", "2026-08-28", &none).unwrap(),
            "render {{user.name}} please"
        );
        assert!(unknown_placeholders("{{{{user.name}}}}").is_empty());
        assert!(arg_keys("{{{{arg.x}}}} {{arg.y}}") == ["y"]);
        // The escape and a real placeholder side by side.
        assert_eq!(
            expand("{{{{date}}}} is {{date}}", "2026-08-28", &none).unwrap(),
            "{{date}} is 2026-08-28"
        );
    }

    #[test]
    fn escaping_survives_a_round_trip_through_expand() {
        let none = BTreeMap::new();
        for text in [
            "{{user.name}}",
            "a {{ b",
            "}}{{",
            "{{{x}}}",
            "{{date}} and {{arg.k}}",
            "nothing at all",
            "čćž {{x}} 🎉",
            "}}}} {{{{",
        ] {
            let escaped = escape(text);
            assert!(
                unknown_placeholders(&escaped).is_empty(),
                "{text} -> {escaped}"
            );
            assert_eq!(
                expand(&escaped, "2026-08-28", &none).unwrap(),
                text,
                "{text}"
            );
        }
    }

    #[test]
    fn the_arg_keys_a_text_uses_are_the_ones_a_run_can_fill() {
        assert_eq!(
            arg_keys("{{arg.a}} {{date}} {{arg.b}} {{arg.a}}"),
            ["a", "b"]
        );
        assert!(arg_keys("{{arg.}} {{nope}}").is_empty());
    }

    #[test]
    fn arg_pairs_are_parsed_once_each() {
        let parsed =
            parse_args(&["a=1".to_string(), "b=x=y".to_string(), "c=".to_string()]).unwrap();
        assert_eq!(parsed["a"], "1");
        assert_eq!(parsed["b"], "x=y");
        assert_eq!(parsed["c"], "");

        for bad in ["nope", "=1", "a b=1"] {
            assert!(
                parse_args(&[bad.to_string()]).is_err(),
                "{bad} was accepted"
            );
        }
        let twice = parse_args(&["a=1".to_string(), "a=2".to_string()]).unwrap_err();
        assert!(twice.to_string().contains("twice"), "{twice}");
    }

    fn full_template() -> Template {
        let mut t = Template::new("weekly-hygiene");
        t.description = Some("the Monday routine".to_string());
        t.cwd = Some("/tmp/work".to_string());
        t.workflow = Some("routine".to_string());
        t.goal = Some("tidy the repo, {{date}}".to_string());
        t.master_prompt = Some("line one\nline two".to_string());
        t.beads_repo = Some("work".to_string());
        t.create_brain = true;
        t.tags = Some(vec!["routine".to_string(), "weekly".to_string()]);
        t.run_count = 12;
        t.last_run_at = Some(1_700_000_000);
        t
    }

    #[test]
    fn a_definition_round_trips_through_toml() {
        let original = full_template();
        let text = render(std::slice::from_ref(&original)).unwrap();
        let back = parse(&text).unwrap();
        assert_eq!(back.templates.len(), 1);

        let mut row = Template::new("placeholder");
        row.run_count = 12;
        row.last_run_at = Some(1_700_000_000);
        back.templates[0].apply(&mut row);
        assert_eq!(Definition::of(&row), Definition::of(&original));
        // The history is the database's, and never the file's.
        assert!(!text.contains("run_count"), "{text}");
        assert!(!text.contains("last_run_at"), "{text}");
        assert!(!text.contains(&original.id), "{text}");
    }

    #[test]
    fn a_blank_field_and_a_missing_one_are_the_same_thing() {
        let doc =
            parse("[[template]]\nname = \"a\"\ngoal = \"  \"\ntags = [\"\", \" x \", \"x\"]\n")
                .unwrap();
        let mut row = Template::new("a");
        doc.templates[0].apply(&mut row);
        assert_eq!(row.goal, None);
        assert_eq!(row.description, None);
        assert_eq!(row.cwd, None);
        assert!(!row.create_brain);
        assert_eq!(row.tags, Some(vec!["x".to_string()]));
    }

    #[test]
    fn every_template_lands_in_one_document() {
        let text = render(&[Template::new("a"), Template::new("b")]).unwrap();
        let names: Vec<String> = parse(&text)
            .unwrap()
            .templates
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(parse("").unwrap(), Document::default());
    }

    /// An empty export is still an export, and has to import again.
    #[test]
    fn an_empty_list_of_templates_is_not_the_same_as_no_list_at_all() {
        let empty = render(&[]).unwrap();
        assert!(declares_templates(&empty), "{empty}");
        assert!(parse(&empty).unwrap().templates.is_empty());
        assert!(!declares_templates("# nothing\n"));
        assert!(!declares_templates("not toml ["));
        assert!(declares_templates("[[template]]\nname = \"a\"\n"));
    }

    #[test]
    fn an_unknown_key_is_a_parse_error_rather_than_a_silent_drop() {
        let e = parse("[[template]]\nname = \"a\"\nrun_count = 9\n").unwrap_err();
        assert!(e.to_string().contains("run_count"), "{e}");
        let e = parse("[[template]]\ngoal = 3\n").unwrap_err();
        assert!(e.to_string().contains("invalid TOML"), "{e}");
    }
}
