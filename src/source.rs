//! Named connection sources: credential resolution and connecting.
//!
//! Credentials never live in `seedle.yaml`. A source names a `.env` file and a
//! variable inside it; we read that file directly rather than mutating the
//! process environment, so two sources can define the same variable name
//! without stepping on each other.

use std::collections::BTreeMap;
use std::path::Path;

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
    let storage = match &src.storage {
        None => None,
        Some(sc) => Some(resolve_storage(cfg, name, src, sc)?),
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
/// The key is read the same way the database password is — from the source's
/// `.env`, never from the config file — so a service-role key never lands in
/// version control.
fn resolve_storage(
    cfg: &Config,
    name: &str,
    src: &SourceConfig,
    sc: &crate::config::StorageConfig,
) -> Result<crate::storage::StorageAccess> {
    let vars = match &src.env_file {
        Some(f) => read_env_file(&cfg.base_dir.join(f)).unwrap_or_default(),
        None => BTreeMap::new(),
    };
    let lookup = |var: &str| -> Option<String> {
        vars.get(var)
            .cloned()
            .or_else(|| std::env::var(var).ok())
            .filter(|v| !v.trim().is_empty())
    };

    let base_url = match (&sc.url, &sc.url_var) {
        (Some(u), _) => u.clone(),
        (None, Some(var)) => lookup(var).ok_or_else(|| {
            anyhow::anyhow!("source {name:?}: storage url variable {var} is not set")
        })?,
        (None, None) => bail!(
            "source {name:?}: `storage` needs either `url` or `url_var` (the project base \
             URL, e.g. http://127.0.0.1:54321)"
        ),
    };

    let key_var = sc.key_var.as_deref().unwrap_or("SUPABASE_SERVICE_ROLE_KEY");
    let key = lookup(key_var).ok_or_else(|| {
        anyhow::anyhow!(
            "source {name:?}: storage key variable {key_var} is not set.\n\
             Storage listing and writing need the service-role key, not the anon key."
        )
    })?;

    Ok(crate::storage::StorageAccess {
        base_url: base_url.trim_end_matches('/').to_string(),
        key,
    })
}

fn resolve_url(cfg: &Config, name: &str, src: &SourceConfig) -> Result<(String, String)> {
    if let Some(url) = &src.url {
        return Ok((url.clone(), "config `url`".to_string()));
    }

    let var = src.url_var.as_deref().unwrap_or(DEFAULT_URL_VAR);

    if let Some(env_file) = &src.env_file {
        let path = cfg.base_dir.join(env_file);
        let vars = read_env_file(&path)?;
        if let Some(url) = vars.get(var) {
            if url.trim().is_empty() {
                bail!(
                    "source {name:?}: {var} is set but empty in {}",
                    path.display()
                );
            }
            return Ok((url.clone(), format!("{} ({var})", path.display())));
        }
        // The env file exists but lacks the variable — fall back to the process
        // environment, which is how CI usually supplies it.
        if let Ok(url) = std::env::var(var) {
            return Ok((url, format!("process env ({var})")));
        }
        bail!(
            "source {name:?}: {var} not found in {} nor in the environment.\n\
             Variables present in that file: {}",
            path.display(),
            if vars.is_empty() {
                "(none)".to_string()
            } else {
                vars.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        );
    }

    match std::env::var(var) {
        Ok(url) if !url.trim().is_empty() => Ok((url, format!("process env ({var})"))),
        _ => bail!(
            "source {name:?}: {var} is not set in the environment and the source \
             defines no `env_file`"
        ),
    }
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
    let ok = match engine {
        Engine::Postgres => matches!(scheme, "postgres" | "postgresql"),
        Engine::Mysql => matches!(scheme, "mysql" | "mariadb"),
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
