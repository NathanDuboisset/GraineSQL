//! An in-place progress line for long exports.
//!
//! Everything goes to stderr, so piped stdout and `--json` stay clean, and the
//! decision to print at all is made once at construction rather than per step:
//! a stray carriage return in a CI log is the classic regression here.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Redraw at most this often, so a fast table does not spend its time in
/// `write`.
const REDRAW: Duration = Duration::from_millis(100);

const MAX_WIDTH: usize = 80;

pub struct Progress {
    enabled: bool,
    total: usize,
    done: usize,
    label: String,
    drawn: usize,
    last: Option<Instant>,
}

impl Progress {
    pub fn new(ctx: &crate::commands::Ctx, total: usize) -> Progress {
        Progress {
            enabled: should_draw(ctx),
            total,
            done: 0,
            label: String::new(),
            drawn: 0,
            last: None,
        }
    }

    /// Move to the next item.
    pub fn step(&mut self, label: &str) {
        if !self.enabled {
            return;
        }
        self.done += 1;
        self.label = label.to_string();
        self.last = None;
        self.draw(None);
    }

    /// Redraw with a running row count for the current item.
    pub fn rows(&mut self, rows: u64) {
        if !self.enabled {
            return;
        }
        if self.last.is_some_and(|t| t.elapsed() < REDRAW) {
            return;
        }
        self.draw(Some(rows));
    }

    /// Erase the line, so whatever writes to stderr next starts clean.
    pub fn clear(&mut self) {
        if !self.enabled || self.drawn == 0 {
            return;
        }
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{:width$}\r", "", width = self.drawn);
        let _ = err.flush();
        self.drawn = 0;
    }

    fn draw(&mut self, rows: Option<u64>) {
        let width = self.total.to_string().len();
        let mut line = format!("[{:>width$}/{}] {}", self.done, self.total, self.label);
        if let Some(n) = rows {
            line.push_str(&format!(" ({n} rows)"));
        }
        // By chars, not bytes: a multibyte table name must not be cut in half.
        let line: String = line.chars().take(MAX_WIDTH).collect();

        let mut err = std::io::stderr();
        let pad = self.drawn.saturating_sub(line.chars().count());
        let _ = write!(err, "\r{line}{:pad$}", "", pad = pad);
        let _ = err.flush();
        self.drawn = line.chars().count();
        self.last = Some(Instant::now());
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}

fn should_draw(ctx: &crate::commands::Ctx) -> bool {
    // `-v` already prints a scrolling line per table, and a rewritten line on
    // top of that interleaves badly. `--json` consumers merge stderr often
    // enough that anything here would corrupt the parse.
    if ctx.quiet || ctx.json || ctx.verbose {
        return false;
    }
    if !std::io::stderr().is_terminal() {
        return false;
    }
    // A pty under CI is still a CI log, and TERM=dumb cannot take a carriage
    // return.
    if std::env::var_os("CI").is_some() {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}
