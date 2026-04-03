//! Signal handling for trap execution.
//!
//! When a trap is set for a signal (e.g. `trap 'cleanup' INT`), we install a
//! signal handler that sets a global atomic flag. The shell checks these flags
//! between commands and runs the corresponding trap action.

use std::sync::atomic::{AtomicBool, Ordering};

static SIGINT_PENDING: AtomicBool = AtomicBool::new(false);
static SIGTERM_PENDING: AtomicBool = AtomicBool::new(false);
static SIGHUP_PENDING: AtomicBool = AtomicBool::new(false);

/// Map a signal name (e.g. "INT", "SIGINT") to its libc signal number.
pub fn name_to_signal(name: &str) -> Option<i32> {
    // Strip "SIG" prefix if present
    let name = name.strip_prefix("SIG").unwrap_or(name);
    match name {
        "HUP" => Some(libc::SIGHUP),
        "INT" => Some(libc::SIGINT),
        "QUIT" => Some(libc::SIGQUIT),
        "ILL" => Some(libc::SIGILL),
        "TRAP" => Some(libc::SIGTRAP),
        "ABRT" | "IOT" => Some(libc::SIGABRT),
        "FPE" => Some(libc::SIGFPE),
        "KILL" => Some(libc::SIGKILL),
        "BUS" => Some(libc::SIGBUS),
        "SEGV" => Some(libc::SIGSEGV),
        "SYS" => Some(libc::SIGSYS),
        "PIPE" => Some(libc::SIGPIPE),
        "ALRM" => Some(libc::SIGALRM),
        "TERM" => Some(libc::SIGTERM),
        "URG" => Some(libc::SIGURG),
        "STOP" => Some(libc::SIGSTOP),
        "TSTP" => Some(libc::SIGTSTP),
        "CONT" => Some(libc::SIGCONT),
        "CHLD" => Some(libc::SIGCHLD),
        "TTIN" => Some(libc::SIGTTIN),
        "TTOU" => Some(libc::SIGTTOU),
        "USR1" => Some(libc::SIGUSR1),
        "USR2" => Some(libc::SIGUSR2),
        _ => None,
    }
}

/// Map a signal number to its name (without SIG prefix).
pub fn signal_to_name(signum: i32) -> Option<&'static str> {
    match signum {
        libc::SIGHUP => Some("HUP"),
        libc::SIGINT => Some("INT"),
        libc::SIGQUIT => Some("QUIT"),
        libc::SIGILL => Some("ILL"),
        libc::SIGTRAP => Some("TRAP"),
        libc::SIGABRT => Some("ABRT"),
        libc::SIGFPE => Some("FPE"),
        libc::SIGKILL => Some("KILL"),
        libc::SIGBUS => Some("BUS"),
        libc::SIGSEGV => Some("SEGV"),
        libc::SIGSYS => Some("SYS"),
        libc::SIGPIPE => Some("PIPE"),
        libc::SIGALRM => Some("ALRM"),
        libc::SIGTERM => Some("TERM"),
        libc::SIGURG => Some("URG"),
        libc::SIGSTOP => Some("STOP"),
        libc::SIGTSTP => Some("TSTP"),
        libc::SIGCONT => Some("CONT"),
        libc::SIGCHLD => Some("CHLD"),
        libc::SIGTTIN => Some("TTIN"),
        libc::SIGTTOU => Some("TTOU"),
        libc::SIGUSR1 => Some("USR1"),
        libc::SIGUSR2 => Some("USR2"),
        _ => None,
    }
}

/// Install a signal handler that sets the pending flag for the given signal.
/// Call this when a trap is set for a signal.
pub fn install_handler(signum: i32) {
    // SAFETY: sigaction is async-signal-safe. The handler only sets an atomic flag.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(signum, &sa, std::ptr::null_mut());
    }
}

/// Reset a signal to its default disposition.
/// Call this when a trap is removed for a signal.
pub fn reset_handler(signum: i32) {
    // SAFETY: sigaction with SIG_DFL is always safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(signum, &sa, std::ptr::null_mut());
    }
}

/// Set a signal to be ignored (for `trap '' SIG`).
pub fn ignore_signal(signum: i32) {
    // SAFETY: sigaction with SIG_IGN is always safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(signum, &sa, std::ptr::null_mut());
    }
}

/// Check and clear all pending signal flags. Returns signal names that were pending.
pub fn take_pending() -> Vec<&'static str> {
    let mut pending = Vec::new();
    if SIGINT_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("INT");
    }
    if SIGTERM_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("TERM");
    }
    if SIGHUP_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("HUP");
    }
    pending
}

extern "C" fn signal_handler(signum: i32) {
    match signum {
        libc::SIGINT => SIGINT_PENDING.store(true, Ordering::Relaxed),
        libc::SIGTERM => SIGTERM_PENDING.store(true, Ordering::Relaxed),
        libc::SIGHUP => SIGHUP_PENDING.store(true, Ordering::Relaxed),
        _ => {}
    }
}
