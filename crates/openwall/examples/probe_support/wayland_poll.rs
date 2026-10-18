// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Result, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{os::unix::net::UnixStream, time::Instant};

pub fn wait<State: 'static>(
    queue: &mut wayland_client::EventQueue<State>,
    connection: &wayland_client::Connection,
    reader: &UnixStream,
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
    let mut fds = [
        PollFd::new(connection, PollFlags::IN),
        PollFd::new(reader, PollFlags::IN),
    ];
    match poll(&mut fds, Some(&timeout)) {
        Ok(_)
            if fds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
        {
            bail!("private Wayland compositor disconnected")
        }
        Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
            guard.read()?;
        }
        Ok(_) | Err(rustix::io::Errno::INTR) => drop(guard),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
