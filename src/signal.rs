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
    match name {
        "INT" | "SIGINT" => Some(libc::SIGINT),
        "TERM" | "SIGTERM" => Some(libc::SIGTERM),
        "HUP" | "SIGHUP" => Some(libc::SIGHUP),
        _ => None,
    }
}

/// Install a signal handler that sets the pending flag for the given signal.
/// Call this when a trap is set for a signal.
pub fn install_handler(signum: i32) {
    // SAFETY: sigaction is async-signal-safe. The handler only sets an atomic flag.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as usize;
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
