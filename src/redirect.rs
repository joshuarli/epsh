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
                    let filepath = self.resolve_path(&filename);
                    let file = std::fs::File::open(&filepath).map_err(|e| {
                        self.err_msg(&format!("{filename}: {e}"));
                        ShellError::Io(e)
                    })?;
                    // SAFETY: file fd is valid from File::open; target_fd is the redirect target.
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Output(word) | RedirKind::Clobber(word) => {
                    let filename = self.expand_string(word)?;
                    let filepath = self.resolve_path(&filename);
                    let file = std::fs::File::create(&filepath).map_err(|e| {
                        self.err_msg(&format!("{filename}: {e}"));
                        ShellError::Io(e)
                    })?;
                    // SAFETY: file fd is valid from File::create; target_fd is the redirect target.
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Append(word) => {
                    let filename = self.expand_string(word)?;
                    let filepath = self.resolve_path(&filename);
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .append(true)
                        .open(&filepath)
                        .map_err(|e| {
                            self.err_msg(&format!("{filename}: {e}"));
                            ShellError::Io(e)
                        })?;
                    // SAFETY: file fd is valid from OpenOptions::open; target_fd is the redirect target.
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::ReadWrite(word) => {
                    let filename = self.expand_string(word)?;
                    let filepath = self.resolve_path(&filename);
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&filepath)
                        .map_err(|e| {
                            self.err_msg(&format!("{filename}: {e}"));
                            ShellError::Io(e)
                        })?;
                    // SAFETY: file fd is valid from OpenOptions::open; target_fd is the redirect target.
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word)?;
                    if fd_str == "-" {
                        // SAFETY: target_fd is a valid fd number from the redirect syntax.
                        unsafe {
                            sys::close(target_fd);
                        }
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        // SAFETY: source_fd is user-specified (may fail at OS level); target_fd is from redirect syntax.
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
                    // SAFETY: fds is a valid 2-element array for pipe() to write into.
                    unsafe {
                        sys::pipe(fds.as_mut_ptr());
                    }
                    // SAFETY: fds[1] is a valid fd just returned by pipe().
                    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                    let read_fd = fds[0];

                    let expanded = match body {
                        HereDocBody::Literal(s) => s.clone(),
                        HereDocBody::Parsed(parts) => {
                            let word = Word {
                                parts: parts.clone(),
                                span: redir.span,
                            };
                            self.expand_string(&word)?
                        }
                    };
                    let _ = (&write_end).write_all(&crate::encoding::str_to_bytes(&expanded));
                    drop(write_end);

                    // When sinks are active the shell is embedded in a multi-threaded
                    // process. Calling dup2(read_fd, 0) in the parent would replace the
                    // process-wide fd 0 and race with other threads reading stdin (e.g.
                    // nerv's keyboard input loop). Instead, stash the read fd and let
                    // eval_external pass it via cmd.stdin() after fork — only the child
                    // inherits it. For builtins/functions that need stdin, the dup2 path
                    // is still used since they run in-process on this thread.
                    if target_fd == 0 && (self.stdout_sink.is_some() || self.stderr_sink.is_some()) {
                        // Close any previously pending stdin (shouldn't happen in practice).
                        if let Some(old) = self.pending_stdin.take() {
                            // SAFETY: old is a valid fd we own.
                            unsafe { sys::close(old); }
                        }
                        self.pending_stdin = Some(read_fd);
                        // No SavedFd entry needed: we never touched fd 0.
                    } else {
                        // SAFETY: read_fd is valid from pipe(); target_fd is the redirect target.
                        unsafe {
                            sys::dup2(read_fd, target_fd);
                            sys::close(read_fd);
                        }
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
                // SAFETY: copy is a valid fd from fcntl_dupfd_cloexec; target_fd is the original fd being restored.
                unsafe {
                    sys::dup2(copy, s.target_fd);
                    sys::close(copy);
                }
            }
        }
    }
}
