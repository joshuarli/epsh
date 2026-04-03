//! Thin wrappers around libc for the syscalls we use.
//! Provides safe-ish Rust signatures and waitpid status helpers.

pub use libc::{close, dup2, execvp, fork, getuid, isatty, pipe, read, umask, waitpid, write};

use std::os::unix::io::RawFd;

pub fn fcntl_dupfd_cloexec(fd: RawFd, min_fd: RawFd) -> i32 {
    unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min_fd) }
}

pub fn wifexited(status: i32) -> bool {
    libc::WIFEXITED(status)
}

pub fn wexitstatus(status: i32) -> i32 {
    libc::WEXITSTATUS(status)
}

pub fn wtermsig(status: i32) -> i32 {
    libc::WTERMSIG(status)
}

/// Exit the process, flushing Rust's stdout first.
/// Raw `_exit()` skips Rust's BufWriter flush, which loses buffered output
/// when fd 1 is a pipe (e.g. in command substitution or pipelines).
pub fn exit_child(status: crate::error::ExitStatus) -> ! {
    {
        let mut out = std::io::stdout().lock();
        let _ = std::io::Write::flush(&mut out);
    }
    unsafe { libc::_exit(status.code()) }
}
