// SPDX-License-Identifier: AGPL-3.0-or-later
//! Input fixture for scripts/test_plasma_bridge.py, not a wallpaper feature.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::{env, thread, time::Duration};
    use wayland_client::{
        Connection, Dispatch, QueueHandle, delegate_noop,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_pointer::ButtonState, wl_registry},
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(State: ignore ZwlrVirtualPointerV1);

    // Explicit test opt-in prevents an accidental cargo run clicking the user's desktop.
    let runtime = env::var_os("XDG_RUNTIME_DIR");
    anyhow::ensure!(
        runtime.is_some() && runtime == env::var_os("OPENWALL_TEST_POINTER_RUNTIME"),
        "this input fixture requires the private test runtime"
    );
    let coordinates = env::args()
        .skip(1)
        .map(|arg| arg.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        coordinates.len() == 4,
        "expected x y extent-width extent-height"
    );
    let [x, y, width, height] = coordinates[..] else {
        unreachable!()
    };
    anyhow::ensure!(
        x < width && y < height,
        "pointer position exceeds private output extent"
    );
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&queue.handle(), 1..=2, ())?;
    let pointer = manager.create_virtual_pointer(None, &queue.handle(), ());
    queue.roundtrip(&mut State)?;
    // Both nested KWin and Plasma must bind their wl_pointer after capabilities arrive.
    thread::sleep(Duration::from_millis(500));
    pointer.motion_absolute(0, x, y, width, height);
    pointer.frame();
    connection.flush()?;
    thread::sleep(Duration::from_millis(500));
    pointer.button(1, 0x110, ButtonState::Pressed);
    pointer.frame();
    connection.flush()?;
    thread::sleep(Duration::from_millis(50));
    pointer.button(2, 0x110, ButtonState::Released);
    pointer.frame();
    queue.roundtrip(&mut State)?;
    thread::sleep(Duration::from_millis(300));
    pointer.destroy();
    manager.destroy();
    connection.flush()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This fixture requires a private Linux Wayland test session");
    std::process::exit(1);
}
