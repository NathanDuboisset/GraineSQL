//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "graine",
    version,
    about = "Deterministic, git-friendly database seed export and load",
    long_about = "GraineSQL exports selected rows from a source database into diffable seed files, \
                  and loads them back into another database in foreign-key order.\n\n\
                  A graine.lock file records the schema the seed files were written against. \
                  Every command checks the live database against it and aborts on breaking drift \
                  before touching any data."
)]
pub struct Cli {
    /// Path to graine.yaml. Defaults to the nearest one in this or a parent directory.
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

    /// Answer yes to every confirmation prompt.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a starter graine.yaml, optionally pre-filled from a live database.
    Init {
        /// Connection URL to introspect for the initial table list.
        #[arg(long)]
        url: Option<String>,

        /// Overwrite an existing graine.yaml.
        #[arg(long)]
        force: bool,
    },

    /// Add tables to the config, along with the parents they depend on.
    Add {
        /// Tables to add.
        #[arg(required = true)]
        tables: Vec<String>,

        /// Add only what was named, without the foreign-key parents it needs.
        #[arg(long)]
        no_parents: bool,
    },

    /// Summarise whether the seed files are current against a database.
    Status,

    /// List configured sources and check that each one connects.
    Sources {
        /// Skip the connection check and only show how credentials resolve.
        #[arg(long)]
        no_connect: bool,
    },

    /// Print a shell completion script.
    Completions {
        /// Shell to generate for.
        shell: clap_complete::Shell,
    },

    /// Introspect the source and write graine.lock.
    Lock {
        /// Exit non-zero if the live schema differs from the lock, without
        /// writing. The CI form.
        #[arg(long)]
        check: bool,
    },

    /// Show how the live schema differs from graine.lock.
    Diff,

    /// Export rows and storage buckets from the source into seed files.
    Export {
        /// Only these tables. Names match the config key or the bare table name.
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Only these storage buckets.
        #[arg(long, value_delimiter = ',', conflicts_with = "no_buckets")]
        buckets: Option<Vec<String>>,

        /// Skip storage buckets entirely.
        #[arg(long)]
        no_buckets: bool,

        /// Override the output format for this run.
        #[arg(long)]
        format: Option<crate::config::Format>,

        /// Override the output directory for this run.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,

        /// Export even if the lock is missing or the schema has drifted.
        #[arg(long)]
        force: bool,

        /// Skip the check that every exported foreign key resolves inside the
        /// export. The files may then fail to load into an empty database.
        #[arg(long)]
        no_fk_check: bool,
    },

    /// Show the load order, row counts, and per-table action. Writes nothing.
    Plan {
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Draw the foreign-key dependencies as a tree, so the load order
        /// explains itself.
        #[arg(long)]
        tree: bool,
    },

    /// Load seed files into the target database and its storage buckets.
    #[command(alias = "seed")]
    Load {
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Only these storage buckets.
        #[arg(long, value_delimiter = ',', conflicts_with = "no_buckets")]
        buckets: Option<Vec<String>>,

        /// Skip storage buckets entirely.
        #[arg(long)]
        no_buckets: bool,

        /// Do everything except commit.
        #[arg(long)]
        dry_run: bool,

        /// Run without a wrapping transaction. Faster on very large loads, but a
        /// failure leaves the database partly written.
        #[arg(long)]
        no_transaction: bool,

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
        let cli = Cli::try_parse_from(["graine", "export", "--tables", "a,b,c"]).unwrap();
        let Command::Export { tables, .. } = cli.command else {
            panic!("expected export")
        };
        assert_eq!(tables.unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["graine", "export", "--source", "prod"]).unwrap();
        assert_eq!(cli.source.as_deref(), Some("prod"));
    }

    #[test]
    fn seed_is_an_alias_for_load() {
        let cli = Cli::try_parse_from(["graine", "seed"]).unwrap();
        assert!(matches!(cli.command, Command::Load { .. }));
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["graine", "-v", "-q", "diff"]).is_err());
    }

    #[test]
    fn bucket_selection_and_skipping_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from(["graine", "export", "--buckets", "a", "--no-buckets"]).is_err()
        );
        let cli = Cli::try_parse_from(["graine", "export", "--buckets", "a,b"]).unwrap();
        let Command::Export { buckets, .. } = cli.command else {
            panic!("expected export")
        };
        assert_eq!(buckets.unwrap(), ["a", "b"]);
    }

    #[test]
    fn load_defaults_are_the_safe_ones() {
        let cli = Cli::try_parse_from(["graine", "load"]).unwrap();
        let Command::Load {
            dry_run,
            no_transaction,
            force,
            ..
        } = cli.command
        else {
            panic!("expected load")
        };
        assert!(!dry_run);
        assert!(!cli.yes, "confirmation must be required unless asked for");
        assert!(!force, "drift must block unless overridden");
        assert!(
            !no_transaction,
            "a load must be atomic unless the user opts out"
        );
    }
}
