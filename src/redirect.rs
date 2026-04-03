use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use crate::ast::*;
use crate::error::ShellError;
use crate::eval::Shell;

use crate::sys;

/// Saved file descriptor for restoration after redirections.
pub(crate) struct SavedFd {
    pub(crate) target_fd: RawFd,
    pub(crate) saved_copy: Option<RawFd>,
}

impl Shell {
    /// Apply redirections. Returns saved FD state for restoration.
    pub(crate) fn setup_redirections(
        &mut self,
        redirs: &[Redir],
    ) -> crate::error::Result<Vec<SavedFd>> {
        let mut saved = Vec::new();

        for redir in redirs {
            let target_fd = redir.fd;

            // Save the current fd
            let saved_copy = {
                let copy = sys::fcntl_dupfd_cloexec(target_fd, 10);
                if copy >= 0 { Some(copy) } else { None }
            };

            saved.push(SavedFd {
                target_fd,
                saved_copy,
            });

            match &redir.kind {
                RedirKind::Input(word) => {
                    let filename = self.expand_string(word)?;
                    let file = std::fs::File::open(&filename).map_err(|e| {
                        eprintln!("{filename}: {e}");
                        ShellError::Io(e)
                    })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Output(word) | RedirKind::Clobber(word) => {
                    let filename = self.expand_string(word)?;
                    let file = std::fs::File::create(&filename).map_err(|e| {
                        eprintln!("{filename}: {e}");
                        ShellError::Io(e)
                    })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Append(word) => {
                    let filename = self.expand_string(word)?;
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .append(true)
                        .open(&filename)
                        .map_err(|e| {
                            eprintln!("{filename}: {e}");
                            ShellError::Io(e)
                        })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::ReadWrite(word) => {
                    let filename = self.expand_string(word)?;
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&filename)
                        .map_err(|e| {
                            eprintln!("{filename}: {e}");
                            ShellError::Io(e)
                        })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        unsafe {
                            sys::close(target_fd);
                        }
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        unsafe {
                            sys::dup2(source_fd, target_fd);
                        }
                    } else {
                        return Err(ShellError::Runtime {
                            msg: format!("{fd_str}: bad file descriptor"),
                            span: redir.span,
                        });
                    }
                }
                RedirKind::HereDoc(body) | RedirKind::HereDocStrip(body) => {
                    let mut fds = [0i32; 2];
                    unsafe {
                        sys::pipe(fds.as_mut_ptr());
                    }
                    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                    let read_fd = fds[0];

                    let expanded = match body {
                        HereDocBody::Literal(s) => s.clone(),
                        HereDocBody::Parsed(parts) => {
                            let word = Word { parts: parts.clone(), span: redir.span };
                            self.expand_string(&word)?
                        }
                    };
                    let _ = (&write_end).write_all(expanded.as_bytes());
                    drop(write_end);

                    unsafe {
                        sys::dup2(read_fd, target_fd);
                        sys::close(read_fd);
                    }
                }
            }
        }

        Ok(saved)
    }

    /// Restore file descriptors after redirections.
    pub(crate) fn restore_redirections(&self, saved: Vec<SavedFd>) {
        for s in saved.into_iter().rev() {
            if let Some(copy) = s.saved_copy {
                unsafe {
                    sys::dup2(copy, s.target_fd);
                    sys::close(copy);
                }
            }
        }
    }
}
