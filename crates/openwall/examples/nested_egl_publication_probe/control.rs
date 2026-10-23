// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{ErrorKind, Read},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::{UnixDatagram, UnixStream},
    },
    path::Path,
};

pub enum Command {
    SetPause {
        reason: openwall::playback::PauseReason,
        paused: bool,
    },
    Publish,
    Rollback,
    Abort,
    Finalize,
}

enum Transport {
    Datagram(UnixDatagram),
    Stream(UnixStream),
}
pub struct Control {
    transport: Transport,
    buffered: Vec<u8>,
    closed: bool,
}
impl Control {
    pub fn bind(path: &Path) -> Result<Self> {
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            transport: Transport::Datagram(socket),
            buffered: Vec::new(),
            closed: false,
        })
    }
    #[allow(dead_code)] // The video diagnostic alone exercises controller lifetime.
    pub fn stdin() -> Result<Self> {
        let socket = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        socket
            .peer_addr()
            .context("control stdin must be a connected Unix stream")?;
        ensure!(
            rustix::net::sockopt::socket_type(&socket)? == rustix::net::SocketType::STREAM,
            "control stdin must be a Unix stream socket"
        );
        ensure!(
            rustix::net::sockopt::socket_peercred(&socket)?.uid == rustix::process::geteuid(),
            "control stdin peer must have the same effective user ID"
        );
        socket.set_nonblocking(true)?;
        Ok(Self {
            transport: Transport::Stream(socket),
            buffered: Vec::new(),
            closed: false,
        })
    }
    #[allow(dead_code)]
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    #[allow(dead_code)]
    pub fn close(&mut self) {
        self.closed = true;
        if let Transport::Stream(socket) = &self.transport {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
    #[allow(dead_code)] // Stream video control must drain coalesced commands before polling.
    pub fn has_buffered_command(&self) -> bool {
        !self.closed && self.buffered.contains(&b'\n')
    }
    pub fn next(&mut self) -> Result<Option<Command>> {
        let mut message = [0; 32];
        if self.closed {
            return Ok(None);
        }
        match &mut self.transport {
            Transport::Datagram(socket) => match socket.recv(&mut message) {
                Ok(count) => Self::parse(&message[..count]).map(Some),
                Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error.into()),
            },
            Transport::Stream(socket) => {
                match socket.read(&mut message) {
                    Ok(0) => {
                        self.closed = true;
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                    }
                    Ok(count) => {
                        ensure!(
                            self.buffered.len() + count <= 64,
                            "invalid nested diagnostic command"
                        );
                        self.buffered.extend_from_slice(&message[..count]);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(None),
                    Err(error) => return Err(error.into()),
                }
                if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                    let command = Self::parse(&self.buffered[..=end])?;
                    self.buffered.drain(..=end);
                    return Ok(Some(command));
                }
                ensure!(
                    !self.closed || self.buffered.is_empty(),
                    "truncated nested diagnostic command"
                );
                Ok(None)
            }
        }
    }
    fn parse(message: &[u8]) -> Result<Command> {
        use openwall::playback::PauseReason;
        match message {
            b"pause user\n" => Ok(Command::SetPause {
                reason: PauseReason::User,
                paused: true,
            }),
            b"resume user\n" => Ok(Command::SetPause {
                reason: PauseReason::User,
                paused: false,
            }),
            b"pause power\n" => Ok(Command::SetPause {
                reason: PauseReason::PowerPolicy,
                paused: true,
            }),
            b"resume power\n" => Ok(Command::SetPause {
                reason: PauseReason::PowerPolicy,
                paused: false,
            }),
            b"publish\n" => Ok(Command::Publish),
            b"rollback\n" => Ok(Command::Rollback),
            b"abort\n" => Ok(Command::Abort),
            b"finalize\n" => Ok(Command::Finalize),
            _ => bail!("invalid nested diagnostic command"),
        }
    }
}
impl AsFd for Control {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.transport {
            Transport::Datagram(socket) => socket.as_fd(),
            Transport::Stream(socket) => socket.as_fd(),
        }
    }
}
