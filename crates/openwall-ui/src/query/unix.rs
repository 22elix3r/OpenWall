// SPDX-License-Identifier: AGPL-3.0-or-later
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use std::{
    collections::VecDeque,
    io::{self, Read},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const ERROR_LIMIT: usize = 16 * 1024;

struct Process {
    child: Child,
    group: Pid,
    reaped: bool,
}

impl Process {
    fn exited(&self) -> io::Result<bool> {
        Ok(waitid(
            WaitId::Pid(self.group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?
        .is_some())
    }

    fn finish(&mut self) -> io::Result<ExitStatus> {
        // We are the sole waiter. WNOWAIT preserves the leader's PID until after
        // group termination, including when a descendant retains a pipe. Never
        // signal a numeric group ID after reaping its leader and allowing reuse.
        let _ = kill_process_group(self.group, Signal::KILL);
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

struct Pipe<R> {
    reader: R,
    bytes: VecDeque<u8>,
    limit: usize,
    tail: bool,
    eof: bool,
}

impl<R: Read + AsFd> Pipe<R> {
    fn new(reader: R, limit: usize, tail: bool) -> io::Result<Self> {
        let flags = fcntl_getfl(&reader)?;
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK)?;
        Ok(Self {
            reader,
            bytes: VecDeque::new(),
            limit,
            tail,
            eof: false,
        })
    }

    fn drain(&mut self) -> io::Result<()> {
        // A noisy stream cannot starve the other pipe or the process deadline.
        for _ in 0..4 {
            let mut bytes = [0; 4096];
            match self.reader.read(&mut bytes) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => {
                    if !self.tail && self.bytes.len() + count > self.limit {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "player query returned too much data",
                        ));
                    }
                    let excess = (self.bytes.len() + count).saturating_sub(self.limit);
                    self.bytes.drain(..excess);
                    self.bytes.extend(&bytes[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub fn run(
    command: &mut Command,
    timeout: Duration,
    cancellation: &super::Cancellation,
) -> io::Result<Output> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid player query timeout")
        })?;
    let child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let group = Pid::from_child(&child);
    let mut process = Process {
        child,
        group,
        reaped: false,
    };
    let mut stdout = Pipe::new(
        process.child.stdout.take().expect("piped stdout"),
        OUTPUT_LIMIT,
        false,
    )?;
    let mut stderr = Pipe::new(
        process.child.stderr.take().expect("piped stderr"),
        ERROR_LIMIT,
        true,
    )?;
    loop {
        if super::is_closing() || cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "player query was cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "player query timed out",
            ));
        }
        stdout.drain()?;
        stderr.drain()?;
        if process.exited()? && stdout.eof && stderr.eof {
            return Ok(Output {
                status: process.finish()?,
                stdout: stdout.bytes.into_iter().collect(),
                stderr: stderr.bytes.into_iter().collect(),
            });
        }
        let mut fds = Vec::with_capacity(2);
        if !stdout.eof {
            fds.push(PollFd::new(&stdout.reader, PollFlags::IN));
        }
        if !stderr.eof {
            fds.push(PollFd::new(&stderr.reader, PollFlags::IN));
        }
        let interval = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        let timeout = Timespec::try_from(interval).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid query poll interval")
        })?;
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}
