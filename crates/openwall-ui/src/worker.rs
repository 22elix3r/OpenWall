// SPDX-License-Identifier: AGPL-3.0-or-later
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "worker/unix.rs"]
mod platform;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use platform::Worker;

// Desktop playback is currently qualified only for these two platform families.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub struct Worker;
    impl Worker {
        pub fn spawn(_: &mut std::process::Command) -> std::io::Result<Self> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "playback workers require Linux or macOS",
            ))
        }
        pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            Ok(None)
        }
        pub fn diagnostic(&self) -> String {
            String::new()
        }
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use platform::Worker;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn noisy_worker_cannot_block_on_a_full_diagnostic_pipe() {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import sys; sys.stderr.write('x' * 2000000 + 'FINAL-DIAGNOSTIC')",
        ]);
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
        assert!(worker.diagnostic().ends_with("FINAL-DIAGNOSTIC"));
    }

    #[test]
    fn exit_and_drop_terminate_ordinary_descendants_and_join_the_reader() {
        for early_exit in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let parent_file = directory.path().join("parent");
            let descendant_file = directory.path().join("descendant");
            let mut command = Command::new("python3");
            command.args(["-c", "import os, pathlib, subprocess, sys, time; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); pathlib.Path(sys.argv[2]).write_text(str(child.pid)); sys.exit(7) if sys.argv[3]=='exit' else time.sleep(60)"])
                .arg(&parent_file).arg(&descendant_file).arg(if early_exit { "exit" } else { "sleep" });
            let mut worker = Worker::spawn(&mut command).unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            let descendant = loop {
                if let Some(pid) = std::fs::read_to_string(&descendant_file)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                {
                    break pid;
                }
                assert!(Instant::now() < until, "descendant fixture did not start");
                std::thread::sleep(Duration::from_millis(10));
            };
            if early_exit {
                let status = loop {
                    if let Some(status) = worker.try_wait().unwrap() {
                        break status;
                    }
                    assert!(
                        Instant::now() < until,
                        "worker did not finish with inherited stderr open"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert_eq!(status.code(), Some(7));
                assert_eq!(worker.try_wait().unwrap(), Some(status));
            }
            // In the running case, Drop must also wake/join a reader parked in poll.
            drop(worker);
            assert!(
                Instant::now() < until,
                "worker cleanup did not wake its reader"
            );
            let parent = std::fs::read_to_string(parent_file)
                .unwrap()
                .parse::<i32>()
                .unwrap();
            assert!(matches!(
                rustix::process::waitpid(
                    rustix::process::Pid::from_raw(parent),
                    rustix::process::WaitOptions::NOHANG
                ),
                Err(rustix::io::Errno::CHILD)
            ));
            loop {
                let status = Command::new("ps")
                    .args(["-p", &descendant.to_string(), "-o", "stat="])
                    .output()
                    .unwrap();
                if !status.status.success()
                    || String::from_utf8_lossy(&status.stdout)
                        .trim_start()
                        .starts_with('Z')
                {
                    break;
                }
                assert!(Instant::now() < until, "worker descendant remains running");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn dropping_a_worker_with_closed_diagnostics_reaps_it() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("python3");
        command.args(["-c", "import os, pathlib, sys, time; os.close(2); pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)"]).arg(&pid_file);
        let worker = Worker::spawn(&mut command).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.parse::<i32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(worker);
        assert!(Instant::now() < until);
        assert!(matches!(
            rustix::process::waitpid(
                rustix::process::Pid::from_raw(pid),
                rustix::process::WaitOptions::NOHANG
            ),
            Err(rustix::io::Errno::CHILD)
        ));
    }
}
