// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Result, bail};
use std::{
    io::ErrorKind,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixDatagram,
    },
    path::Path,
};

pub enum Command {
    Publish,
    Rollback,
    Abort,
    Finalize,
}

pub struct Control(UnixDatagram);
impl Control {
    pub fn bind(path: &Path) -> Result<Self> {
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        Ok(Self(socket))
    }
    pub fn next(&self) -> Result<Option<Command>> {
        let mut message = [0; 32];
        match self.0.recv(&mut message) {
            Ok(count) => match &message[..count] {
                b"publish\n" => Ok(Some(Command::Publish)),
                b"rollback\n" => Ok(Some(Command::Rollback)),
                b"abort\n" => Ok(Some(Command::Abort)),
                b"finalize\n" => Ok(Some(Command::Finalize)),
                _ => bail!("invalid nested diagnostic command"),
            },
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
impl AsFd for Control {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
