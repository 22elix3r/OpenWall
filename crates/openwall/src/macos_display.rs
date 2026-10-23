// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-only display discovery and unambiguous selection on the AppKit main thread.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    monitor::MonitorHandle,
    platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS, MonitorHandleExtMacOS},
    window::WindowId,
};

#[derive(Serialize)]
pub struct Display {
    name: String,
    label: String,
    selector: String,
    width: u32,
    height: u32,
    scale: f64,
    x: i32,
    y: i32,
    primary: bool,
}

#[derive(Default)]
struct Discovery {
    outputs: Option<Vec<Display>>,
}

impl ApplicationHandler for Discovery {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let primary = event_loop.primary_monitor();
        let mut monitors: Vec<_> = event_loop.available_monitors().collect();
        monitors.sort_by_key(|monitor| (primary.as_ref() != Some(monitor), monitor.native_id()));
        self.outputs = Some(
            monitors
                .iter()
                .enumerate()
                .map(|(index, monitor)| {
                    let size = monitor.size();
                    let position = monitor.position();
                    let name = monitor.name().unwrap_or_else(|| "Display".into());
                    Display {
                        label: format!(
                            "{} · {} × {} · Display {}",
                            name,
                            size.width,
                            size.height,
                            index + 1
                        ),
                        name,
                        selector: format!("macos:{}", monitor.native_id()),
                        width: size.width,
                        height: size.height,
                        scale: monitor.scale_factor(),
                        x: position.x,
                        y: position.y,
                        primary: primary.as_ref() == Some(monitor),
                    }
                })
                .collect(),
        );
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

pub fn outputs() -> Result<Vec<Display>> {
    // This CLI runs in its own process, on main. It creates no windows and must
    // not activate above the calling GUI or appear in the Dock.
    let event_loop = EventLoop::<()>::with_user_event()
        .with_activation_policy(ActivationPolicy::Prohibited)
        .with_activate_ignoring_other_apps(false)
        .build()?;
    let mut discovery = Discovery::default();
    event_loop.run_app(&mut discovery)?;
    discovery
        .outputs
        .context("display discovery did not initialize")
}

pub fn select(event_loop: &ActiveEventLoop, selector: &str) -> Result<MonitorHandle> {
    let monitors: Vec<_> = event_loop.available_monitors().collect();
    let identities: Vec<_> = monitors
        .iter()
        .map(|monitor| (monitor.native_id(), monitor.name()))
        .collect();
    let index = select_index(&identities, selector)?;
    Ok(monitors[index].clone())
}

fn select_index(identities: &[(u32, Option<String>)], selector: &str) -> Result<usize> {
    let native_id = selector
        .strip_prefix("macos:")
        .map(str::parse::<u32>)
        .transpose()
        .context("invalid macOS display identifier")?;
    let mut matches = identities
        .iter()
        .enumerate()
        .filter(|(_, (id, name))| {
            native_id.map_or_else(|| name.as_deref() == Some(selector), |native| *id == native)
        })
        .map(|(index, _)| index);
    let selected = matches
        .next()
        .with_context(|| format!("display not found: {selector}"))?;
    if matches.next().is_some() {
        bail!("ambiguous display name: {selector}; use an identifier from openwall outputs");
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::select_index;

    #[test]
    fn same_named_displays_require_distinct_identifiers() {
        let monitors = [
            (42, Some("Same monitor".into())),
            (87, Some("Same monitor".into())),
        ];
        assert!(
            select_index(&monitors, "Same monitor")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert_eq!(select_index(&monitors, "macos:42").unwrap(), 0);
        assert_eq!(select_index(&monitors, "macos:87").unwrap(), 1);
        assert!(select_index(&monitors, "macos:99").is_err());
        assert!(select_index(&monitors, "macos:invalid").is_err());
    }

    #[test]
    fn unique_names_and_unnamed_displays_are_supported() {
        let monitors = [(42, Some("Unique".into())), (87, None)];
        assert_eq!(select_index(&monitors, "Unique").unwrap(), 0);
        assert_eq!(select_index(&monitors, "macos:87").unwrap(), 1);
        assert!(select_index(&monitors, "").is_err());
        assert!(select_index(&[], "macos:42").is_err());
    }
}
