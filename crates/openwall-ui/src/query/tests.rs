// SPDX-License-Identifier: AGPL-3.0-or-later
use super::*;
use rustix::process::Pid;
use std::path::Path;

fn run(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    crate::query::run(command, timeout)
}

fn python(code: &str) -> Command {
    let mut command = Command::new("python3");
    command.args(["-c", code]);
    command
}

fn reaped(path: &Path) -> Pid {
    let pid = std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = Pid::from_raw(pid).unwrap();
    assert!(matches!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
        Err(rustix::io::Errno::CHILD)
    ));
    pid
}

#[test]
fn drains_noisy_stderr_and_returns_complete_stdout_and_status() {
    let output = run(
        &mut python(
            "import sys; sys.stderr.write('x' * 2000000 + 'TAIL'); print('{\"ready\":true}')",
        ),
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["ready"],
        true
    );
    assert_eq!(output.stderr.len(), ERROR_LIMIT);
    assert!(output.stderr.ends_with(b"TAIL"));
    let failure = run(
        &mut python("import sys; sys.stderr.write('decoder unavailable'); sys.exit(7)"),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(failure.status.code(), Some(7));
    assert_eq!(failure.stderr, b"decoder unavailable");
}

#[test]
fn accepts_the_output_limit_but_rejects_overflow_and_reaps_the_worker() {
    let output = run(
        &mut python("import sys; sys.stdout.buffer.write(b'x' * 65536)"),
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), OUTPUT_LIMIT);
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let mut command = python(
        "import os, pathlib, sys; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); sys.stdout.buffer.write(b'x' * 65537); sys.stdout.flush()",
    );
    command.arg(&pid_file);
    let error = run(&mut command, Duration::from_secs(5)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    reaped(&pid_file);
}

#[test]
fn deadline_kills_and_reaps_a_worker_even_when_both_pipes_close_early() {
    for close_pipes in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = python(
            "import os, pathlib, signal, sys, time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); signal.signal(signal.SIGINT, signal.SIG_IGN); close=sys.argv[2]=='close'; (os.close(1),os.close(2)) if close else None; time.sleep(60)",
        );
        command
            .arg(&pid_file)
            .arg(if close_pipes { "close" } else { "open" });
        let started = Instant::now();
        let error = run(&mut command, Duration::from_millis(750)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        reaped(&pid_file);
    }
}

#[test]
fn inherited_pipes_do_not_outlive_the_deadline_or_leave_running_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("parent");
    let descendant_file = directory.path().join("descendant");
    let mut command = python(
        "import os, pathlib, subprocess, sys; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); pathlib.Path(sys.argv[2]).write_text(str(child.pid))",
    );
    command.arg(&pid_file).arg(&descendant_file);
    let error = run(&mut command, Duration::from_millis(750)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    reaped(&pid_file);
    let descendant = std::fs::read_to_string(descendant_file).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let status = Command::new("ps")
            .args(["-p", descendant.trim(), "-o", "stat="])
            .output()
            .unwrap();
        if !status.status.success()
            || String::from_utf8_lossy(&status.stdout)
                .trim_start()
                .starts_with('Z')
        {
            break; // An orphan may await its system reaper; it must no longer run.
        }
        assert!(Instant::now() < until, "query descendant remains running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn invalid_deadline_does_not_spawn_a_worker() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("must-not-exist");
    let mut command = python("import pathlib, sys; pathlib.Path(sys.argv[1]).touch()");
    command.arg(&file);
    assert_eq!(
        run(&mut command, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(!file.exists());
}

#[test]
fn cancelled_queries_reap_workers_without_cancelling_other_queries() {
    let directory = tempfile::tempdir().unwrap();
    let cancellation = crate::query::Cancellation::default();
    cancellation.cancel();
    let marker = directory.path().join("not-spawned");
    let mut command = python("import pathlib, sys; pathlib.Path(sys.argv[1]).touch()");
    command.arg(&marker);
    assert_eq!(
        crate::query::run_cancellable(&mut command, Duration::from_secs(60), &cancellation)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert!(!marker.exists(), "already-cancelled query spawned a child");

    for close_pipes in [false, true] {
        let pid_file = directory.path().join(format!("worker-{close_pipes}"));
        let worker_file = pid_file.clone();
        let cancellation = crate::query::Cancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::spawn(move || {
            let mut command = python(
                "import os, pathlib, sys, time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); (os.close(1),os.close(2)) if sys.argv[2]=='close' else None; time.sleep(60)",
            );
            command
                .arg(worker_file)
                .arg(if close_pipes { "close" } else { "open" });
            crate::query::run_cancellable(
                &mut command,
                Duration::from_secs(60),
                &worker_cancellation,
            )
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() {
            assert!(Instant::now() < until, "cancellable worker did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        cancellation.cancel();
        // A fresh query must remain usable while the obsolete one is cancelled.
        let output = run(&mut python("print('fresh')"), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"fresh\n");
        while !worker.is_finished() {
            assert!(
                Instant::now() < until,
                "cancelled query kept its worker alive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        reaped(&pid_file);
    }
}

#[test]
fn query_scope_bounds_jobs_and_shutdown_reaps_active_children() {
    // Scope shutdown is process-global, just like GUI exit. Isolate it from
    // parallel tests instead of resetting a production lifecycle invariant.
    const CHILD: &str = "OPENWALL_QUERY_SCOPE_TEST";
    if std::env::var_os(CHILD).is_none() {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "query::platform::tests::query_scope_bounds_jobs_and_shutdown_reaps_active_children"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let release = Arc::new(AtomicBool::new(false));
    for _ in 0..8 {
        let release = release.clone();
        crate::query::spawn(move || {
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .unwrap();
    }
    assert_eq!(
        crate::query::spawn(|| {}).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    release.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(5);
    while crate::query::spawn(|| {}).is_err() {
        assert!(
            Instant::now() < until,
            "completed jobs did not release query slots"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let worker_file = pid_file.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    crate::query::spawn(move || {
        let mut command = python("import os, pathlib, sys, time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)");
        command.arg(worker_file);
        let error = crate::query::run(&mut command, Duration::from_secs(60)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        worker_cancelled.store(true, Ordering::Release);
    }).unwrap();
    while !pid_file.exists() {
        assert!(Instant::now() < until, "query worker did not initialize");
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::query::shutdown();
    assert!(cancelled.load(Ordering::Acquire));
    reaped(&pid_file);
    assert_eq!(
        crate::query::spawn(|| {}).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}
