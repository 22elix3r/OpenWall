// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sole-waiter ownership of a child process group on macOS/Linux.
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use std::{
    io,
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Stdio},
};

pub struct Process {
    pub child: Child,
    group: Pid,
    reaped: bool,
}

impl Process {
    pub fn spawn(command: &mut Command, stdout: Stdio) -> io::Result<Self> {
        let child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self {
            group: Pid::from_child(&child),
            child,
            reaped: false,
        })
    }

    pub fn exited(&self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        Ok(waitid(
            WaitId::Pid(self.group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?
        .is_some())
    }

    pub fn finish(&mut self) -> io::Result<ExitStatus> {
        if !self.reaped {
            // WNOWAIT keeps the leader's PID reserved until group termination.
            // Never signal this numeric PGID after reaping allows its reuse.
            let _ = kill_process_group(self.group, Signal::KILL);
        }
        let result = self.child.wait();
        self.reaped = true;
        result
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.finish();
        }
    }
}
