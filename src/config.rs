//! `seedle.yaml` parsing, validation, and per-table defaults resolution.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::schema::TableId;

pub const CONFIG_FILENAME: &str = "seedle.yaml";
pub const LOCK_FILENAME: &str = "seedle.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
        }
    }
}

/// Output format for a table's seed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum Format {
    Jsonl,
    Csv,
    Sql,
    /// Only valid with `layout: per_row` — a whole file is one JSON object.
    Json,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Jsonl => "jsonl",
            Format::Csv => "csv",
            Format::Sql => "sql",
            Format::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// One file for the whole table.
    Single,
    /// One file per row, in a directory named after the table.
    PerRow,
}

/// How JSON/JSONB columns are represented in JSON output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonMode {
    /// Embed as a nested JSON value — readable and diffable.
    Unroll,
    /// Keep as an escaped JSON string, byte-identical to what the DB returned.
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Insert, updating the non-key columns of rows that already exist.
    Upsert,
    /// Plain insert; a conflict aborts.
    Insert,
    /// Insert, silently ignoring rows that already exist.
    SkipExisting,
    /// Delete every row in the table first, then insert.
    TruncateFirst,
}

impl LoadMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LoadMode::Upsert => "upsert",
            LoadMode::Insert => "insert",
            LoadMode::SkipExisting => "skip_existing",
            LoadMode::TruncateFirst => "truncate_first",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub engine: Engine,
    /// Path to a `.env` holding the credentials, relative to the config file.
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    /// Variable inside `env_file` (or the process env) holding the URL.
    #[serde(default)]
    pub url_var: Option<String>,
    /// Literal connection URL. Discouraged — it puts credentials in git.
    #[serde(default)]
    pub url: Option<String>,
    /// Refuse any write (`load`) against this source.
    #[serde(default)]
    pub read_only: bool,
    /// The source used when `--source` is omitted.
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Directory holding the lock file and the seed files.
    #[serde(default = "default_out")]
    pub out: PathBuf,
    #[serde(default = "default_format")]
    pub format: Format,
    #[serde(default = "default_json_mode")]
    pub json: JsonMode,
    #[serde(default = "default_sql_batch")]
    pub sql_batch: usize,
    /// Number of tables exported concurrently. Cannot affect output bytes.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_out() -> PathBuf {
    PathBuf::from("seed")
}
fn default_format() -> Format {
    Format::Jsonl
}
fn default_json_mode() -> JsonMode {
    JsonMode::Unroll
}
fn default_sql_batch() -> usize {
    100
}
fn default_concurrency() -> usize {
    4
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            out: default_out(),
            format: default_format(),
            json: default_json_mode(),
            sql_batch: default_sql_batch(),
            concurrency: default_concurrency(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadConfig {
    #[serde(default = "default_load_mode", rename = "default")]
    pub mode: LoadMode,
    /// Wrap the whole load in one transaction.
    #[serde(default = "default_true")]
    pub transaction: bool,
    /// Advance sequences / AUTO_INCREMENT past the loaded keys afterwards.
    #[serde(default = "default_true")]
    pub fix_sequences: bool,
}

fn default_load_mode() -> LoadMode {
    LoadMode::Upsert
}
fn default_true() -> bool {
    true
}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            mode: default_load_mode(),
            transaction: true,
            fix_sequences: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableConfig {
    /// Raw SQL predicate in the source dialect, spliced into the `WHERE` clause.
    #[serde(default, rename = "where")]
    pub filter: Option<String>,
    #[serde(default)]
    pub order_by: Vec<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    /// Explicit column allowlist. Mutually exclusive with `exclude_columns`.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_columns: Vec<String>,
    #[serde(default)]
    pub format: Option<Format>,
    #[serde(default)]
    pub layout: Option<Layout>,
    #[serde(default)]
    pub pretty: Option<bool>,
    #[serde(default)]
    pub json: Option<JsonMode>,
    #[serde(default, rename = "load")]
    pub load_mode: Option<LoadMode>,
    /// Override the upsert conflict target. Defaults to the primary key.
    #[serde(default)]
    pub key: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub sources: IndexMap<String, SourceConfig>,
    #[serde(default)]
    pub export: ExportConfig,
    #[serde(default)]
    pub load: LoadConfig,
    #[serde(default)]
    pub tables: IndexMap<String, TableConfig>,

    /// Directory the config was loaded from. Every relative path in the config
    /// resolves against this, not against the process working directory.
    #[serde(skip, default)]
    pub base_dir: PathBuf,
}

/// A table's settings after merging global defaults with its own overrides.
#[derive(Debug, Clone)]
pub struct ResolvedTable {
    pub id: TableId,
    /// The key as written in the config, for error messages.
    pub config_key: String,
    pub filter: Option<String>,
    pub order_by: Vec<String>,
    pub limit: Option<u64>,
    pub columns: Option<Vec<String>>,
    pub exclude_columns: Vec<String>,
    pub format: Format,
    pub layout: Layout,
    pub pretty: bool,
    pub json: JsonMode,
    pub load_mode: LoadMode,
    pub key: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Find `seedle.yaml` in `start` or any ancestor directory.
    pub fn discover(start: &Path) -> Result<PathBuf> {
        let start = start
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        for dir in start.ancestors() {
            let candidate = dir.join(CONFIG_FILENAME);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        bail!(
            "no {CONFIG_FILENAME} found in {} or any parent directory (run `seedle init`)",
            start.display()
        )
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported config version {} (this build understands version 1)",
                self.version
            );
        }
        if self.sources.is_empty() {
            bail!("config has no sources; at least one is required");
        }

        let defaults: Vec<&str> = self
            .sources
            .iter()
            .filter(|(_, s)| s.default)
            .map(|(n, _)| n.as_str())
            .collect();
        if defaults.len() > 1 {
            bail!(
                "sources {} are all marked `default: true`; exactly one may be",
                defaults.join(", ")
            );
        }

        for (name, src) in &self.sources {
            if src.url.is_none() && src.env_file.is_none() && src.url_var.is_none() {
                bail!(
                    "source {name:?} specifies no credentials: set one of `url`, \
                     `env_file`, or `url_var`"
                );
            }
        }

        if self.export.sql_batch == 0 {
            bail!("export.sql_batch must be at least 1");
        }
        if self.export.concurrency == 0 {
            bail!("export.concurrency must be at least 1");
        }

        for (key, t) in &self.tables {
            let id: TableId = key
                .parse()
                .map_err(|e| anyhow::anyhow!("table key {key:?}: {e}"))?;
            let _ = id;

            if t.columns.is_some() && !t.exclude_columns.is_empty() {
                bail!(
                    "table {key:?} sets both `columns` and `exclude_columns`; \
                     use one or the other"
                );
            }
            if let Some(cols) = &t.columns {
                if cols.is_empty() {
                    bail!("table {key:?} has an empty `columns` list");
                }
                if let Some(dup) = first_duplicate(cols) {
                    bail!("table {key:?} lists column {dup:?} twice in `columns`");
                }
            }
            if let Some(k) = &t.key
                && k.is_empty()
            {
                bail!("table {key:?} has an empty `key` list");
            }
            if t.limit == Some(0) {
                bail!("table {key:?} has `limit: 0`; omit the table instead");
            }

            let layout = t.layout.unwrap_or(Layout::Single);
            let format = t.format.unwrap_or(self.export.format);
            match (layout, format) {
                (Layout::Single, Format::Json) => bail!(
                    "table {key:?} uses `format: json` with the default single-file \
                     layout; json is only valid with `layout: per_row` (did you mean jsonl?)"
                ),
                (Layout::PerRow, Format::Csv | Format::Sql) => bail!(
                    "table {key:?} uses `layout: per_row` with `format: {}`; \
                     per-row output must be json",
                    format.extension()
                ),
                _ => {}
            }
            if t.pretty == Some(true) && !matches!(format, Format::Json) {
                bail!(
                    "table {key:?} sets `pretty: true` but `format: {}`; pretty-printing \
                     requires `format: json` with `layout: per_row` (jsonl is one line per row \
                     by definition — use `json: unroll` for readable nested values there)",
                    format.extension()
                );
            }
        }

        Ok(())
    }

    /// The source to use when `--source` is not given.
    pub fn default_source(&self) -> Result<&str> {
        if let Some((name, _)) = self.sources.iter().find(|(_, s)| s.default) {
            return Ok(name);
        }
        if self.sources.len() == 1 {
            return Ok(self.sources.keys().next().unwrap());
        }
        bail!(
            "no source marked `default: true` and {} sources defined; pass --source",
            self.sources.len()
        )
    }

    pub fn source(&self, name: &str) -> Result<&SourceConfig> {
        self.sources.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown source {name:?}; defined sources: {}",
                self.sources.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// Absolute output directory.
    pub fn out_dir(&self) -> PathBuf {
        self.base_dir.join(&self.export.out)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.out_dir().join(LOCK_FILENAME)
    }

    /// All configured tables with defaults merged in, in config order.
    pub fn resolved_tables(&self) -> Result<Vec<ResolvedTable>> {
        self.tables
            .iter()
            .map(|(key, t)| {
                let id: TableId = key.parse().map_err(|e| anyhow::anyhow!("{key:?}: {e}"))?;
                let format = t.format.unwrap_or(self.export.format);
                Ok(ResolvedTable {
                    id,
                    config_key: key.clone(),
                    filter: t.filter.clone(),
                    order_by: t.order_by.clone(),
                    limit: t.limit,
                    columns: t.columns.clone(),
                    exclude_columns: t.exclude_columns.clone(),
                    format,
                    layout: t.layout.unwrap_or(Layout::Single),
                    pretty: t.pretty.unwrap_or(matches!(format, Format::Json)),
                    json: t.json.unwrap_or(self.export.json),
                    load_mode: t.load_mode.unwrap_or(self.load.mode),
                    key: t.key.clone(),
                })
            })
            .collect()
    }

    /// Resolved tables filtered by a `--tables a,b` selection.
    ///
    /// Names are matched against the config key and against the bare table name,
    /// so `--tables users` hits a `public.users` entry.
    pub fn select_tables(&self, only: Option<&[String]>) -> Result<Vec<ResolvedTable>> {
        let all = self.resolved_tables()?;
        let Some(only) = only else { return Ok(all) };

        let mut wanted: BTreeSet<&str> = only.iter().map(|s| s.as_str()).collect();
        let mut picked = Vec::new();
        for t in all {
            let hit = wanted
                .iter()
                .find(|w| **w == t.config_key || **w == t.id.name)
                .copied();
            if let Some(w) = hit {
                wanted.remove(w);
                picked.push(t);
            }
        }
        if !wanted.is_empty() {
            let missing: Vec<&str> = wanted.into_iter().collect();
            bail!(
                "--tables named {} which {} not in the config; configured tables: {}",
                missing.join(", "),
                if missing.len() == 1 { "is" } else { "are" },
                self.tables.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        Ok(picked)
    }
}

fn first_duplicate(items: &[String]) -> Option<&String> {
    let mut seen = BTreeSet::new();
    items.iter().find(|i| !seen.insert(i.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Config> {
        let mut cfg: Config = serde_yaml_ng::from_str(yaml)?;
        cfg.base_dir = PathBuf::from(".");
        cfg.validate()?;
        Ok(cfg)
    }

    const MINIMAL: &str = r#"
version: 1
sources:
  dev:
    engine: postgres
    url_var: DATABASE_URL
    default: true
tables:
  users: {}
"#;

    /// `MINIMAL` with extra keys under the `users` table.
    fn with_users_keys(body: &str) -> String {
        let head = MINIMAL
            .trim_end()
            .strip_suffix("users: {}")
            .expect("fixture shape");
        format!("{head}users:\n{body}")
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.default_source().unwrap(), "dev");
        assert_eq!(cfg.export.format, Format::Jsonl);
        assert_eq!(cfg.export.out, PathBuf::from("seed"));
        assert_eq!(cfg.load.mode, LoadMode::Upsert);
        assert!(cfg.load.transaction);

        let t = &cfg.resolved_tables().unwrap()[0];
        assert_eq!(t.id, TableId::bare("users"));
        assert_eq!(t.layout, Layout::Single);
        assert_eq!(t.json, JsonMode::Unroll);
        assert!(!t.pretty);
    }

    #[test]
    fn two_defaults_is_an_error() {
        let yaml = MINIMAL.replace(
            "    default: true",
            "    default: true\n  other:\n    engine: mysql\n    url_var: X\n    default: true",
        );
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("default: true"), "{err}");
    }

    #[test]
    fn single_source_needs_no_default_flag() {
        let yaml = MINIMAL.replace("    default: true\n", "");
        assert_eq!(parse(&yaml).unwrap().default_source().unwrap(), "dev");
    }

    #[test]
    fn ambiguous_source_without_default_errors() {
        let yaml = MINIMAL.replace(
            "    default: true",
            "  other:\n    engine: mysql\n    url_var: X",
        );
        let cfg = parse(&yaml).unwrap();
        assert!(cfg.default_source().is_err());
    }

    #[test]
    fn source_without_credentials_errors() {
        let yaml = "
version: 1
sources:
  dev:
    engine: postgres
tables: {}
";
        let err = parse(yaml).unwrap_err().to_string();
        assert!(err.contains("no credentials"), "{err}");
    }

    #[test]
    fn columns_and_exclude_columns_conflict() {
        let yaml = with_users_keys("    columns: [id]\n    exclude_columns: [x]\n");
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn pretty_requires_per_row_json() {
        let yaml = with_users_keys("    pretty: true\n");
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("pretty"), "{err}");
        assert!(err.contains("one line per row"), "{err}");
    }

    #[test]
    fn json_format_requires_per_row_layout() {
        let yaml = with_users_keys("    format: json\n");
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("per_row"), "{err}");
    }

    #[test]
    fn per_row_rejects_csv() {
        let yaml = with_users_keys("    layout: per_row\n    format: csv\n");
        let err = parse(&yaml).unwrap_err().to_string();
        assert!(err.contains("per-row output must be json"), "{err}");
    }

    #[test]
    fn per_row_json_enables_pretty_by_default() {
        let yaml = with_users_keys("    layout: per_row\n    format: json\n");
        let t = &parse(&yaml).unwrap().resolved_tables().unwrap()[0];
        assert!(t.pretty);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let yaml = with_users_keys("    wehre: \"x = 1\"\n");
        assert!(
            parse(&yaml).is_err(),
            "typo in a table key should not be silently ignored"
        );
    }

    #[test]
    fn select_tables_matches_bare_name_against_qualified_key() {
        let yaml = "
version: 1
sources:
  dev: {engine: postgres, url_var: DATABASE_URL}
tables:
  public.users: {}
  audit.events: {}
";
        let cfg = parse(yaml).unwrap();
        let picked = cfg.select_tables(Some(&["users".to_string()])).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].config_key, "public.users");
    }

    #[test]
    fn select_tables_preserves_config_order_not_argument_order() {
        let yaml = "
version: 1
sources:
  dev: {engine: postgres, url_var: DATABASE_URL}
tables:
  a: {}
  b: {}
  c: {}
";
        let cfg = parse(yaml).unwrap();
        let picked = cfg
            .select_tables(Some(&["c".to_string(), "a".to_string()]))
            .unwrap();
        let names: Vec<&str> = picked.iter().map(|t| t.config_key.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn select_tables_rejects_unknown_name() {
        let cfg = parse(MINIMAL).unwrap();
        let err = cfg
            .select_tables(Some(&["nope".to_string()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn per_table_overrides_beat_globals() {
        let yaml = "
version: 1
sources:
  dev: {engine: postgres, url_var: DATABASE_URL}
export:
  format: csv
load:
  default: upsert
tables:
  users:
    format: jsonl
    load: insert
    where: \"id > 10\"
    limit: 5
";
        let cfg = parse(yaml).unwrap();
        let t = &cfg.resolved_tables().unwrap()[0];
        assert_eq!(t.format, Format::Jsonl);
        assert_eq!(t.load_mode, LoadMode::Insert);
        assert_eq!(t.filter.as_deref(), Some("id > 10"));
        assert_eq!(t.limit, Some(5));
    }
}
