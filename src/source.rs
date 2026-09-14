//! Named connection sources: credential resolution and connecting.
//!
//! Credentials never live in `graine.yaml`. A source names a `.env` file and a
//! variable inside it; we read that file directly rather than mutating the
//! process environment, so two sources can define the same variable name
//! without stepping on each other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{Config, Engine, SourceConfig};

const DEFAULT_URL_VAR: &str = "DATABASE_URL";

/// A source with its credentials resolved to a concrete URL.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub name: String,
    pub engine: Engine,
    pub url: String,
    pub read_only: bool,
    /// Where the URL came from, for display. Never contains the URL itself.
    pub origin: String,
    /// Object storage access, when the source declares it.
    pub storage: Option<crate::storage::StorageAccess>,
}

impl ResolvedSource {
    /// The URL with any password replaced, safe to print or log.
    pub fn redacted_url(&self) -> String {
        redact_url(&self.url)
    }

    /// True when the URL points at this machine. Used to decide whether a
    /// destructive load needs an explicit confirmation.
    pub fn is_local(&self) -> bool {
        let host = url_host(&self.url);
        match host.as_deref() {
            None => true, // unix socket or no host component
            Some(h) => {
                h.is_empty()
                    || h == "localhost"
                    || h == "127.0.0.1"
                    || h == "::1"
                    || h == "[::1]"
                    || h.starts_with("/")
            }
        }
    }
}

/// Resolve a source's credentials without connecting.
pub fn resolve(cfg: &Config, name: &str) -> Result<ResolvedSource> {
    let src = cfg.source(name)?;
    let (url, origin) = resolve_url(cfg, name, src)?;
    check_scheme(name, src.engine, &url)?;
    // A Supabase source gets storage without declaring it.
    let storage = if src.storage.is_some() || src.engine.has_storage() {
        Some(resolve_storage(cfg, name, src, src.storage.as_ref())?)
    } else {
        None
    };
    Ok(ResolvedSource {
        name: name.to_string(),
        engine: src.engine,
        url,
        read_only: src.read_only,
        origin,
        storage,
    })
}

/// Resolve a source's storage URL and service key.
///
/// The key is read the same way the database password is, from the source's
/// `.env`, never from the config file, so a service-role key never lands in
/// version control.
fn resolve_storage(
    cfg: &Config,
    name: &str,
    src: &SourceConfig,
    sc: Option<&crate::config::StorageConfig>,
) -> Result<crate::storage::StorageAccess> {
    let vars = Vars::load(cfg, src)?;

    let base_url = match sc.and_then(|s| s.url.clone()) {
        Some(u) => u,
        None => {
            let named = sc.and_then(|s| s.url_var.clone());
            let candidates: Vec<&str> = match &named {
                Some(v) => vec![v.as_str()],
                None => crate::config::supabase::URL_VARS.to_vec(),
            };
            vars.first(&candidates).map(|(v, _)| v).ok_or_else(|| {
                anyhow::anyhow!(
                    "source {name:?}: no storage URL. Set one of {}, or `storage.url_var`.",
                    candidates.join(", ")
                )
            })?
        }
    };

    let named = sc.and_then(|s| s.key_var.clone());
    let candidates: Vec<&str> = match &named {
        Some(v) => vec![v.as_str()],
        None => crate::config::supabase::KEY_VARS.to_vec(),
    };
    let key = vars.first(&candidates).map(|(v, _)| v).ok_or_else(|| {
        anyhow::anyhow!(
            "source {name:?}: no storage key. Set one of {}, or `storage.key_var`. \
             Listing and writing need the service-role key, not the anon key.",
            candidates.join(", ")
        )
    })?;

    Ok(crate::storage::StorageAccess {
        base_url: base_url.trim_end_matches('/').to_string(),
        key,
    })
}

/// Credentials available to a source: its `.env` file if it has one, plus the
/// process environment as a fallback.
struct Vars {
    file: BTreeMap<String, String>,
    file_path: Option<PathBuf>,
}

impl Vars {
    fn load(cfg: &Config, src: &SourceConfig) -> Result<Vars> {
        match &src.env_file {
            None => Ok(Vars {
                file: BTreeMap::new(),
                file_path: None,
            }),
            Some(f) => {
                let path = cfg.base_dir.join(f);
                Ok(Vars {
                    file: read_env_file(&path)?,
                    file_path: Some(path),
                })
            }
        }
    }

    /// Look one variable up, file first, then the process environment.
    fn get(&self, var: &str) -> Option<(String, String)> {
        if let Some(v) = self.file.get(var).filter(|v| !v.trim().is_empty()) {
            let origin = match &self.file_path {
                Some(p) => format!("{} ({var})", p.display()),
                None => format!("env file ({var})"),
            };
            return Some((v.clone(), origin));
        }
        std::env::var(var)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| (v, format!("process env ({var})")))
    }

    /// First of `candidates` that is set.
    fn first(&self, candidates: &[&str]) -> Option<(String, String)> {
        candidates.iter().find_map(|v| self.get(v))
    }

    fn names(&self) -> String {
        if self.file.is_empty() {
            "(none)".to_string()
        } else {
            self.file.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    }
}

fn resolve_url(cfg: &Config, name: &str, src: &SourceConfig) -> Result<(String, String)> {
    if let Some(url) = &src.url {
        return Ok((url.clone(), "config `url`".to_string()));
    }

    let vars = Vars::load(cfg, src)?;

    // An explicit url_var must be found; otherwise try the engine's usual names.
    if let Some(var) = &src.url_var {
        return vars.get(var).ok_or_else(|| {
            anyhow::anyhow!(
                "source {name:?}: {var} is not set{}.\nVariables in that file: {}",
                match &vars.file_path {
                    Some(p) => format!(" in {} nor in the environment", p.display()),
                    None => " in the environment".to_string(),
                },
                vars.names()
            )
        });
    }

    let candidates: Vec<&str> = if src.engine == crate::config::Engine::Supabase {
        crate::config::supabase::DB_URL_VARS.to_vec()
    } else {
        vec![DEFAULT_URL_VAR]
    };
    vars.first(&candidates).ok_or_else(|| {
        anyhow::anyhow!(
            "source {name:?}: none of {} is set. Set one, or name a variable with \
             `url_var`.",
            candidates.join(", ")
        )
    })
}

/// Parse a `.env` file into a map, leaving the process environment untouched.
fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        bail!("env file {} does not exist", path.display());
    }
    let iter = dotenvy::from_path_iter(path)
        .with_context(|| format!("reading env file {}", path.display()))?;
    let mut out = BTreeMap::new();
    for item in iter {
        let (k, v) = item.with_context(|| format!("parsing env file {}", path.display()))?;
        out.insert(k, v);
    }
    Ok(out)
}

fn check_scheme(name: &str, engine: Engine, url: &str) -> Result<()> {
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("");
    let ok = match engine.dialect() {
        Engine::Mysql => matches!(scheme, "mysql" | "mariadb"),
        // A path with no scheme is the common way to name a database file.
        Engine::Sqlite => scheme.is_empty() || scheme == "sqlite",
        Engine::Postgres | Engine::Supabase => matches!(scheme, "postgres" | "postgresql"),
    };
    if !ok {
        bail!(
            "source {name:?} declares `engine: {}` but its URL scheme is {:?}; \
             the engine and the URL must agree",
            engine.as_str(),
            if scheme.is_empty() { "(none)" } else { scheme }
        );
    }
    Ok(())
}

/// Replace the password in a URL with `***`.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // Authority ends at the first '/', '?' or '#'.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    if userinfo.contains(':') {
        format!("{scheme}://{user}:***@{host}{tail}")
    } else {
        url.to_string()
    }
}

/// Host component of a connection URL, if any.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if host_port.is_empty() {
        return None;
    }
    // Bracketed IPv6, else strip a trailing :port.
    if let Some(end) = host_port.find(']') {
        return Some(host_port[..=end].to_string());
    }
    Some(
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact_url("postgres://alice:s3cret@db.example.com:5432/app"),
            "postgres://alice:***@db.example.com:5432/app"
        );
        // No password to hide.
        assert_eq!(
            redact_url("postgres://alice@localhost/app"),
            "postgres://alice@localhost/app"
        );
        assert_eq!(
            redact_url("postgres:///app?host=/var/run"),
            "postgres:///app?host=/var/run"
        );
    }

    #[test]
    fn redaction_does_not_leak_via_query_string_at() {
        // An '@' after the path must not be mistaken for the userinfo separator.
        let url = "postgres://localhost/app?options=user@host";
        assert_eq!(redact_url(url), url);
    }

    #[test]
    fn extracts_host() {
        assert_eq!(
            url_host("postgres://u:p@db.internal:5432/app").as_deref(),
            Some("db.internal")
        );
        assert_eq!(
            url_host("postgres://localhost/app").as_deref(),
            Some("localhost")
        );
        assert_eq!(url_host("mysql://[::1]:3306/app").as_deref(), Some("[::1]"));
        assert_eq!(url_host("postgres:///app").as_deref(), None);
    }

    #[test]
    fn locality_detection() {
        let mk = |url: &str| ResolvedSource {
            name: "t".into(),
            engine: Engine::Postgres,
            url: url.into(),
            read_only: false,
            origin: "test".into(),
            storage: None,
        };
        assert!(mk("postgres://localhost/app").is_local());
        assert!(mk("postgres://127.0.0.1:5432/app").is_local());
        assert!(mk("postgres://[::1]/app").is_local());
        assert!(mk("postgres:///app?host=/var/run").is_local());
        assert!(!mk("postgres://prod.example.com/app").is_local());
    }

    #[test]
    fn engine_and_scheme_must_agree() {
        assert!(check_scheme("s", Engine::Postgres, "postgres://x/y").is_ok());
        assert!(check_scheme("s", Engine::Postgres, "postgresql://x/y").is_ok());
        assert!(check_scheme("s", Engine::Mysql, "mysql://x/y").is_ok());
        let err = check_scheme("s", Engine::Postgres, "mysql://x/y")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must agree"), "{err}");
    }
}
