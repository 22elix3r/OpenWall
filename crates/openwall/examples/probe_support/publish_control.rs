// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-command diagnostic control, owned by the private compositor harness.
use anyhow::{Result, ensure};
use std::{
    io::ErrorKind,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixDatagram,
    },
    path::Path,
};

pub struct Control {
    socket: UnixDatagram,
    requested: bool,
}

impl Control {
    pub fn bind(path: &Path) -> Result<Self> {
        // Never unlink or replace a caller's existing path. The harness removes
        // its own 0700 temporary directory after the process has been reaped.
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            requested: false,
        })
    }

    pub fn poll(&mut self) -> Result<bool> {
        let mut message = [0; 32];
        loop {
            match self.socket.recv(&mut message) {
                Ok(count) => {
                    ensure!(
                        !self.requested && &message[..count] == b"publish\n",
                        "invalid or duplicate staged publish command"
                    );
                    self.requested = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(self.requested),
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl AsFd for Control {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}
