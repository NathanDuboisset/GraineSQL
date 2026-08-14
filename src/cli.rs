//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "seedle",
    version,
    about = "Deterministic, git-friendly database seed export and load",
    long_about = "seedle exports selected rows from a source database into diffable seed files, \
                  and loads them back into another database in foreign-key order.\n\n\
                  A seedle.lock file records the schema the seed files were written against. \
                  Every command checks the live database against it and aborts on breaking drift \
                  before touching any data."
)]
pub struct Cli {
    /// Path to seedle.yaml. Defaults to the nearest one in this or a parent directory.
    #[arg(long, short = 'c', global = true)]
    pub config: Option<PathBuf>,

    /// Source to use. Defaults to the one marked `default: true`.
    #[arg(long, short = 's', global = true)]
    pub source: Option<String>,

    /// Emit machine-readable JSON instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print more detail, including every generated statement.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Print only errors.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a starter seedle.yaml, optionally pre-filled from a live database.
    Init {
        /// Connection URL to introspect for the initial table list.
        #[arg(long)]
        url: Option<String>,

        /// Overwrite an existing seedle.yaml.
        #[arg(long)]
        force: bool,
    },

    /// List configured sources and check that each one connects.
    Sources {
        /// Skip the connection check and only show how credentials resolve.
        #[arg(long)]
        no_connect: bool,
    },

    /// Introspect the source and write seedle.lock.
    Lock {
        /// Exit non-zero if the live schema differs from the lock, without
        /// writing. The CI form.
        #[arg(long)]
        check: bool,
    },

    /// Show how the live schema differs from seedle.lock.
    Diff,

    /// Export rows from the source into seed files.
    Export {
        /// Only these tables. Names match the config key or the bare table name.
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Override the output format for this run.
        #[arg(long)]
        format: Option<crate::config::Format>,

        /// Override the output directory for this run.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,

        /// Export even if the lock is missing or the schema has drifted.
        #[arg(long)]
        force: bool,
    },

    /// Show the load order, row counts, and per-table action. Writes nothing.
    Plan {
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,
    },

    /// Load seed files into the target database.
    #[command(alias = "seed")]
    Load {
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Do everything except commit.
        #[arg(long)]
        dry_run: bool,

        /// Run without a wrapping transaction. Faster on very large loads, but a
        /// failure leaves the database partly written.
        #[arg(long)]
        no_transaction: bool,

        /// Skip the confirmation prompt for a remote or destructive load.
        #[arg(long, short = 'y')]
        yes: bool,

        /// Load even if the schema has drifted. Dangerous.
        #[arg(long)]
        force: bool,
    },

    /// Check the seed files against the lock. Needs no database.
    Verify,
}

impl Cli {
    /// Resolve the config path, discovering it if not given.
    pub fn config_path(&self) -> anyhow::Result<PathBuf> {
        match &self.config {
            Some(p) => Ok(p.clone()),
            None => crate::config::Config::discover(&std::env::current_dir()?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches conflicting flags, duplicate short options, and bad defaults
        // at test time rather than on first run.
        Cli::command().debug_assert();
    }

    #[test]
    fn tables_accepts_a_comma_separated_list() {
        let cli = Cli::try_parse_from(["seedle", "export", "--tables", "a,b,c"]).unwrap();
        let Command::Export { tables, .. } = cli.command else {
            panic!("expected export")
        };
        assert_eq!(tables.unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["seedle", "export", "--source", "prod"]).unwrap();
        assert_eq!(cli.source.as_deref(), Some("prod"));
    }

    #[test]
    fn seed_is_an_alias_for_load() {
        let cli = Cli::try_parse_from(["seedle", "seed"]).unwrap();
        assert!(matches!(cli.command, Command::Load { .. }));
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["seedle", "-v", "-q", "diff"]).is_err());
    }

    #[test]
    fn load_defaults_are_the_safe_ones() {
        let cli = Cli::try_parse_from(["seedle", "load"]).unwrap();
        let Command::Load {
            dry_run,
            no_transaction,
            yes,
            force,
            ..
        } = cli.command
        else {
            panic!("expected load")
        };
        assert!(!dry_run);
        assert!(!yes, "confirmation must be required unless asked for");
        assert!(!force, "drift must block unless overridden");
        assert!(
            !no_transaction,
            "a load must be atomic unless the user opts out"
        );
    }
}
