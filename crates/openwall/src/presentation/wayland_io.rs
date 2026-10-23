// SPDX-License-Identifier: AGPL-3.0-or-later
//! Libwayland may have another reader (for example Vulkan WSI or GStreamer) on
//! this connection. Poll readability does not guarantee bytes remain for us.
use wayland_client::backend::{ReadEventsGuard, WaylandError};

pub fn read_events(guard: ReadEventsGuard) -> Result<usize, WaylandError> {
    match guard.read() {
        Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wayland_client::Connection;

    #[test]
    fn an_empty_socket_is_recoverable_and_does_not_invalidate_the_connection() {
        let (client, _server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        for _ in 0..3 {
            let guard = connection
                .prepare_read()
                .expect("empty connection can prepare read");
            assert_eq!(read_events(guard).unwrap(), 0);
            assert!(connection.backend().last_error().is_none());
        }
    }

    #[test]
    fn poll_readiness_consumed_by_another_reader_can_be_retried() {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        use std::io::Write;
        let (client, mut server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let guard = connection.prepare_read().unwrap();
        server.write_all(&[0; 4]).unwrap();
        let mut fds = [PollFd::new(&connection, PollFlags::IN)];
        assert_eq!(
            poll(
                &mut fds,
                Some(&Timespec {
                    tv_sec: 0,
                    tv_nsec: 0
                })
            )
            .unwrap(),
            1
        );
        assert!(fds[0].revents().contains(PollFlags::IN));
        // Consume the raw bytes before the event guard reads. No protocol message
        // is dispatched here; this isolates the stale-readiness transport race.
        let mut consumed = [0; 4];
        assert_eq!(rustix::io::read(&connection, &mut consumed).unwrap(), 4);
        assert_eq!(read_events(guard).unwrap(), 0);
        assert!(connection.backend().last_error().is_none());
    }

    #[test]
    fn a_closed_peer_remains_an_error() {
        let (client, server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        drop(server);
        let guard = connection.prepare_read().expect("connection not read yet");
        assert!(
            read_events(guard).is_err(),
            "EOF was treated as temporary unavailability"
        );
    }
}
