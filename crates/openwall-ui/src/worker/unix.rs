// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::process::Process;
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    process::{ChildStderr, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

const ERROR_LIMIT: usize = 16 * 1024;
type Tail = Arc<Mutex<VecDeque<u8>>>;

/// Owns both the process group and its interruptible diagnostic reader.
pub struct Worker {
    process: Process,
    errors: Tail,
    stop: UnixStream,
    reader: Option<JoinHandle<io::Result<()>>>,
    status: Option<ExitStatus>,
}

fn drain(stderr: &mut ChildStderr, tail: &Tail, turns: usize) -> io::Result<bool> {
    for _ in 0..turns {
        let mut bytes = [0; 4096];
        match stderr.read(&mut bytes) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let mut tail = tail.lock().unwrap_or_else(|poison| poison.into_inner());
                let excess = (tail.len() + count).saturating_sub(ERROR_LIMIT);
                tail.drain(..excess);
                tail.extend(&bytes[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn read_errors(mut stderr: ChildStderr, mut stop: UnixStream, tail: Tail) -> io::Result<()> {
    loop {
        let mut stopped = [0; 1];
        match stop.read(&mut stopped) {
            Ok(_) => {
                let _ = drain(&mut stderr, &tail, 64)?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if drain(&mut stderr, &tail, 4)? {
            return Ok(());
        }
        let mut fds = [
            PollFd::new(&stderr, PollFlags::IN),
            PollFd::new(&stop, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

impl Worker {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let (stop, reader_stop) = UnixStream::pair()?;
        reader_stop.set_nonblocking(true)?;
        let mut process = Process::spawn(command, Stdio::null())?;
        let stderr = process.child.stderr.take().expect("piped worker stderr");
        fcntl_setfl(&stderr, fcntl_getfl(&stderr)? | OFlags::NONBLOCK)?;
        let errors = Arc::new(Mutex::new(VecDeque::new()));
        let reader_tail = errors.clone();
        let reader = std::thread::Builder::new()
            .name("openwall-worker-errors".into())
            .spawn(move || read_errors(stderr, reader_stop, reader_tail))?;
        Ok(Self {
            process,
            errors,
            stop,
            reader: Some(reader),
            status: None,
        })
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = self.stop.write_all(&[1]);
            let error = match reader.join() {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("\nWorker diagnostics stopped: {error}"),
                Err(_) => "\nWorker diagnostics stopped unexpectedly".into(),
            };
            let mut tail = self
                .errors
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            tail.extend(error.bytes());
            let excess = tail.len().saturating_sub(ERROR_LIMIT);
            tail.drain(..excess);
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        if !self.process.exited()? {
            return Ok(None);
        }
        let status = self.process.finish();
        self.join_reader();
        let status = status?;
        self.status = Some(status);
        Ok(Some(status))
    }

    pub fn diagnostic(&self) -> String {
        let tail = self
            .errors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.process.finish();
        self.join_reader();
    }
}
