//! Minimal POSIX syscall wrappers using std::os::unix.
//! Avoids depending on the libc crate.

use std::os::unix::io::RawFd;

extern "C" {
    pub fn fork() -> i32;
    pub fn _exit(status: i32) -> !;
    pub fn pipe(fds: *mut i32) -> i32;
    pub fn dup2(oldfd: i32, newfd: i32) -> i32;
    pub fn close(fd: i32) -> i32;
    pub fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    pub fn execvp(file: *const i8, argv: *const *const i8) -> i32;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn umask(mask: u32) -> u32;
    pub fn getuid() -> u32;
    pub fn isatty(fd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

pub const F_DUPFD_CLOEXEC: i32 = {
    #[cfg(target_os = "linux")]
    { 1030 }
    #[cfg(target_os = "macos")]
    { 67 }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { 1030 }
};

pub unsafe fn fcntl_dupfd_cloexec(fd: RawFd, min_fd: RawFd) -> i32 {
    fcntl(fd, F_DUPFD_CLOEXEC, min_fd)
}

// waitpid status macros
pub fn wifexited(status: i32) -> bool {
    (status & 0x7f) == 0
}

pub fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

pub fn wtermsig(status: i32) -> i32 {
    status & 0x7f
}

/// Exit the process, flushing Rust's stdout first.
/// Raw _exit() skips Rust's BufWriter flush, which loses buffered output
/// when fd 1 is a pipe (e.g. in command substitution or pipelines).
pub fn exit_child(status: i32) -> ! {
    {
        let mut out = std::io::stdout().lock();
        let _ = std::io::Write::flush(&mut out);
    }
    unsafe { _exit(status) }
}
