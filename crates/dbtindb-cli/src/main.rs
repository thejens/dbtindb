//! `dbtindb`: the command line companion of the dbtindb extensions.
//!
//! It talks to no database itself. It writes SQL for `psql` or `sqlite3`,
//! so connections, credentials and extension loading stay with the
//! clients people already use.

mod project;
mod script;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::script::{Dialect, Mode};

#[derive(Parser)]
#[command(
    name = "dbtindb",
    version,
    about = "Run dbt projects inside PostgreSQL and SQLite"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print SQL that loads a dbt project into the database's dbtindb_files
    /// table, for piping into psql or sqlite3.
    ///
    ///   dbtindb load ./jaffle_shop --dialect postgres | psql -v ON_ERROR_STOP=1 "$DATABASE_URL"
    ///   dbtindb load ./jaffle_shop --dialect sqlite | sqlite3 app.db
    #[command(verbatim_doc_comment)]
    Load {
        /// The project directory, holding dbt_project.yml.
        #[arg(default_value = ".")]
        project: PathBuf,
        /// The database the script is for.
        #[arg(long, value_enum)]
        dialect: Dialect,
        /// Insert and update files, but keep rows for files that are not in
        /// the directory (by default the table ends up holding exactly the
        /// project).
        #[arg(long)]
        merge: bool,
        /// The table to load into, if not dbtindb_files on the search path.
        #[arg(long, default_value = "dbtindb_files")]
        table: String,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dbtindb: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Load {
            project,
            dialect,
            merge,
            table,
        } => {
            let (files, warnings) = project::collect(&project)?;
            for w in warnings {
                eprintln!("dbtindb: warning: {w}");
            }
            let mode = if merge { Mode::Merge } else { Mode::Replace };
            let origin = project.display().to_string();
            let sql = script::script(dialect, &table, mode, &origin, &files);
            std::io::stdout()
                .lock()
                .write_all(sql.as_bytes())
                .map_err(|e| format!("writing the script: {e}"))?;
            eprintln!("dbtindb: {} files from {origin}", files.len());
            Ok(())
        }
    }
}
