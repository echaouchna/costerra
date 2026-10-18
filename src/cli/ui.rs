//! Terminal progress: one FinOps-flavoured spinner shown from the start of a
//! run (rotating currency notes, "▰▱" meter while prices are fetched).
//! Disabled unless stderr is an interactive terminal with colors.

use indicatif::{ProgressBar, ProgressStyle};
use parking_lot::Mutex;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static BAR: Mutex<Option<ProgressBar>> = Mutex::new(None);

const TICKS: &[&str] = &["💵", "💴", "💶", "💷", "💰"];

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg:.bold} \x1b[2m{elapsed}\x1b[0m")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(TICKS)
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg:.bold} \x1b[32m{bar:24}\x1b[0m {pos:>3}/{len} SKUs \x1b[2m· {elapsed}\x1b[0m")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_strings(TICKS)
        .progress_chars("▰▰▱")
}

/// Show the spinner immediately.
pub fn start(enabled: bool, msg: &str) {
    if !enabled {
        return;
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(msg.to_string());
    pb.tick();
    pb.enable_steady_tick(Duration::from_millis(100));
    *BAR.lock() = Some(pb);
}

/// Switch to a spinner with a new message. While the price meter is running
/// it keeps being shown (it is the long part of the run).
pub fn phase(msg: &str) {
    if let Some(pb) = BAR.lock().as_ref() {
        if pb.length().is_some_and(|l| l > 0 && pb.position() < l) {
            return;
        }
        pb.set_style(spinner_style());
        pb.set_length(0);
        pb.set_message(msg.to_string());
    }
}

/// Switch to the price meter for `len` SKUs (or extend it).
pub fn meter(msg: &str, len: u64) {
    if let Some(pb) = BAR.lock().as_ref() {
        let running = pb.length().is_some_and(|l| l > 0 && pb.position() < l);
        if running {
            pb.inc_length(len);
        } else {
            pb.set_style(bar_style());
            pb.set_position(0);
            pb.set_length(len);
        }
        pb.set_message(msg.to_string());
    }
}

/// A clonable handle to advance the meter from other threads.
pub fn ticker() -> impl Fn() + Send + Sync + 'static {
    let pb = BAR.lock().clone();
    move || {
        if let Some(pb) = &pb {
            pb.inc(1);
        }
    }
}

/// Print a line above the spinner.
pub fn println(line: &str) {
    match BAR.lock().as_ref() {
        Some(pb) => pb.suspend(|| eprintln!("{line}")),
        None => eprintln!("{line}"),
    }
}

/// Remove the spinner (before printing the report).
pub fn clear() {
    if let Some(pb) = BAR.lock().take() {
        pb.finish_and_clear();
    }
}

static STDERR_COLOR: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn init(stderr_color: bool, verbose: bool) {
    STDERR_COLOR.store(stderr_color, Ordering::Relaxed);
    VERBOSE.store(verbose, Ordering::Relaxed);
}

/// The spinner is shown on an interactive, colored stderr, and not with -v
/// (log lines would interleave with it).
pub fn enabled() -> bool {
    STDERR_COLOR.load(Ordering::Relaxed) && std::io::stderr().is_terminal() && !VERBOSE.load(Ordering::Relaxed)
}

/// Print a highlighted message on stderr: `kind` is "error", "warning", "policy" or "info".
pub fn notice(kind: &str, msg: impl std::fmt::Display) {
    let label = format!("{kind}:");
    let label = if STDERR_COLOR.load(Ordering::Relaxed) {
        let code = match kind {
            "error" | "policy" => "1;31",
            "warning" => "1;33",
            _ => "1;36",
        };
        format!("\x1b[{code}m{label}\x1b[0m")
    } else {
        label
    };
    println(&format!("{label} {msg}"));
}
