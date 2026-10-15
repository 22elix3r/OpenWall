// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    collections::VecDeque,
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
};

/// Diagnostics are drained continuously and bounded to the last 16 KiB.
pub struct Worker {
    child: Child,
    errors: Arc<Mutex<VecDeque<u8>>>,
}

impl Worker {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let errors = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let tail = errors.clone();
            std::thread::spawn(move || {
                let mut bytes = [0u8; 1024];
                while let Ok(count) = stderr.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    if let Ok(mut tail) = tail.lock() {
                        tail.extend(&bytes[..count]);
                        while tail.len() > 16 * 1024 {
                            tail.pop_front();
                        }
                    }
                }
            });
        }
        Ok(Self { child, errors })
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    pub fn diagnostic(&self) -> String {
        self.errors
            .lock()
            .map(|x| String::from_utf8_lossy(&x.iter().copied().collect::<Vec<_>>()).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn noisy_worker_cannot_block_on_a_full_diagnostic_pipe() {
        let mut command = Command::new("python3");
        command.args(["-c", "import sys; sys.stderr.write('x' * 2000000)"]);
        let mut worker = Worker::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = worker.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker blocked on stderr backpressure"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.diagnostic().is_empty());
        assert!(worker.diagnostic().len() <= 16 * 1024);
    }
}
