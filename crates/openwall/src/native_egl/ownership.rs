// SPDX-License-Identifier: AGPL-3.0-or-later
use super::gl_api::Api;
use super::window::Session;
use anyhow::{Result, ensure};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Mutex,
};
use wayland_client::Connection;

// Only connection handles cross threads, never EGL contexts or Rc owners.
// A reservation is installed before any EGL initialization and removed after
// native destruction, including all partial-construction error paths.
static OWNERS: Mutex<Vec<Connection>> = Mutex::new(Vec::new());
thread_local! {
    static LOCAL: RefCell<Vec<Weak<Session>>> = const { RefCell::new(Vec::new()) };
}

pub(super) struct Reservation(Connection);
impl Reservation {
    pub fn claim(connection: &Connection) -> Result<Self> {
        let mut owners = OWNERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            !owners
                .iter()
                .any(|owner| owner.backend() == connection.backend()),
            "Wayland connection already has a native EGL owner on another thread or during initialization"
        );
        owners.push(connection.clone());
        Ok(Self(connection.clone()))
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        OWNERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|owner| owner.backend() != self.0.backend());
    }
}

pub(super) fn existing(connection: &Connection) -> Option<Rc<Session>> {
    LOCAL.with(|local| {
        let mut local = local.borrow_mut();
        local.retain(|owner| owner.strong_count() > 0);
        local
            .iter()
            .filter_map(Weak::upgrade)
            .find(|owner| owner.connection().backend() == connection.backend())
    })
}

pub(super) fn remember(owner: &Rc<Session>) {
    LOCAL.with(|local| local.borrow_mut().push(Rc::downgrade(owner)));
}

pub(super) fn check_api(selected: Api) -> Result<()> {
    ensure!(
        LOCAL.with(|local| local
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .all(|owner| owner.selected_api() == selected)),
        "native EGL thread already owns a different GL API"
    );
    Ok(())
}
