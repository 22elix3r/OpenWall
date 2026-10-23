// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Result, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixStream,
    },
    time::Instant,
};

#[allow(dead_code)] // Staged diagnostics also poll a private control socket.
pub fn wait<State: 'static>(
    queue: &mut wayland_client::EventQueue<State>,
    connection: &wayland_client::Connection,
    reader: &UnixStream,
    deadline: Instant,
) -> Result<()> {
    wait_fds(queue, connection, &[reader.as_fd()], deadline)
}

pub fn wait_fds<State: 'static>(
    queue: &mut wayland_client::EventQueue<State>,
    connection: &wayland_client::Connection,
    wakes: &[BorrowedFd<'_>],
    deadline: Instant,
) -> Result<()> {
    connection.flush()?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = Timespec {
        tv_sec: remaining.as_secs() as i64,
        tv_nsec: remaining.subsec_nanos() as i64,
    };
    let mut fds = Vec::with_capacity(1 + wakes.len());
    fds.push(PollFd::new(connection, PollFlags::IN));
    for wake in wakes {
        fds.push(PollFd::new(wake, PollFlags::IN));
    }
    match poll(&mut fds, Some(&timeout)) {
        Ok(_)
            if fds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
        {
            bail!("private Wayland compositor disconnected")
        }
        Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
            openwall::presentation::wayland_io::read_events(guard)?;
        }
        Ok(_) | Err(rustix::io::Errno::INTR) => drop(guard),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
