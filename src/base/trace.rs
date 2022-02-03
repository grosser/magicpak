use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Output};

use crate::base::error;
use crate::base::{Error, Result};

use log::warn;
use nix::libc;

pub trait CommandTraceExt {
    fn traceme(&mut self) -> &mut Command;
}

impl CommandTraceExt for Command {
    fn traceme(&mut self) -> &mut Command {
        unsafe { self.pre_exec(|| nix::sys::ptrace::traceme().map_err(error::nix_to_io)) }
    }
}

pub struct SyscallHandler<FOpen, FOpenAt> {
    pub open: FOpen,
    pub openat: FOpenAt,
}

pub trait ChildTraceExt {
    fn trace_syscalls<FOpen, FOpenAt>(
        self,
        handler: SyscallHandler<FOpen, FOpenAt>,
    ) -> Result<Output>
    where
        FOpen: FnMut(OsString, i32),
        FOpenAt: FnMut(i32, OsString, i32);
}

impl ChildTraceExt for Child {
    fn trace_syscalls<FOpen, FOpenAt>(
        mut self,
        mut handler: SyscallHandler<FOpen, FOpenAt>,
    ) -> Result<Output>
    where
        FOpen: FnMut(OsString, i32),
        FOpenAt: FnMut(i32, OsString, i32),
    {
        use nix::sys::signal::Signal;
        use nix::sys::wait::WaitStatus;

        let child_pid = nix::unistd::Pid::from_raw(self.id() as i32);

        match nix::sys::wait::waitpid(child_pid, None)? {
            WaitStatus::Stopped(_, Signal::SIGTRAP) => (),
            WaitStatus::Signaled(_, sig, _) | WaitStatus::Stopped(_, sig) => {
                return Err(Error::DynamicSignaled(sig))
            }
            WaitStatus::Exited(_, code) => {
                let status = ExitStatus::from_raw(code);
                return output_of_child(&mut self, status);
            }
            _ => unreachable!(),
        }

        // TODO: should we handle forks?
        use nix::sys::ptrace::Options;
        nix::sys::ptrace::setoptions(
            child_pid,
            Options::PTRACE_O_TRACESYSGOOD | Options::PTRACE_O_EXITKILL,
        )?;
        nix::sys::ptrace::syscall(child_pid, None)?;

        loop {
            match nix::sys::wait::waitpid(child_pid, None)? {
                WaitStatus::Exited(_, code) => {
                    let status = ExitStatus::from_raw(code);
                    return output_of_child(&mut self, status);
                }
                WaitStatus::Stopped(pid, sig) => {
                    warn!(
                        "trace_syscalls: stopped with {}, we attempt to continue",
                        sig
                    );
                    nix::sys::ptrace::syscall(pid, None)?;
                }
                WaitStatus::PtraceSyscall(pid) => {
                    let regs = getregs(pid)?;
                    match regs.orig_rax as i64 {
                        libc::SYS_openat => {
                            let dirfd = regs.rdi as i32;
                            let pathname = read_string_at(pid, regs.rsi)?;
                            let flags = regs.rdx as i32;
                            (handler.openat)(dirfd, pathname, flags);
                        }
                        libc::SYS_open => {
                            let pathname = read_string_at(pid, regs.rdi)?;
                            let flags = regs.rsi as i32;
                            (handler.open)(pathname, flags);
                        }
                        _ => (),
                    }
                    nix::sys::ptrace::syscall(pid, None)?;
                }
                WaitStatus::Signaled(_, sig, _) => return Err(Error::DynamicSignaled(sig)),
                _ => unreachable!(),
            }
        }
    }
}

fn output_of_child(child: &mut Child, status: ExitStatus) -> Result<Output> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut child_stdout) = child.stdout.take() {
        child_stdout.read_to_end(&mut stdout)?;
    }
    if let Some(mut child_stderr) = child.stderr.take() {
        child_stderr.read_to_end(&mut stderr)?;
    }
    let output = Output {
        status,
        stdout,
        stderr,
    };
    Ok(output)
}

fn getregs(pid: nix::unistd::Pid) -> Result<libc::user_regs_struct> {
    nix::sys::ptrace::getregs(pid).map_err(Into::into)
}

fn read_string_at(pid: nix::unistd::Pid, mut addr: u64) -> Result<OsString> {
    use std::ffi::c_void;

    let mut result = Vec::new();
    loop {
        let word = nix::sys::ptrace::read(pid, addr as *mut c_void)? as u32;
        let bytes: [u8; 4] = word.to_ne_bytes();
        for byte in bytes.iter() {
            if *byte == 0 {
                return Ok(OsString::from_vec(result));
            }
            result.push(*byte);
        }
        addr += 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_cmd::prelude::*;
    use assert_fs::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn test_trace() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let test_path = assert_fs::NamedTempFile::new("test")?;
        test_path.touch()?;
        let child = Command::new("cat")
            .arg(test_path.path())
            .traceme()
            .spawn()?;

        let paths = Rc::new(RefCell::new(Vec::new()));
        child
            .trace_syscalls(SyscallHandler {
                open: |pathname, _| paths.borrow_mut().push(pathname),
                openat: |_, pathname, _| paths.borrow_mut().push(pathname),
            })?
            .assert()
            .success();

        assert_eq!(
            true,
            Rc::try_unwrap(paths)
                .unwrap()
                .into_inner()
                .iter()
                .any(|p| p == test_path.path())
        );
        Ok(())
    }
}
