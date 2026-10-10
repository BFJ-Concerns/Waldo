//! The interaction fixtures every toolkit phase runs, how each is launched,
//! and what admits one to the campaign.

use std::{
    path::Path,
    process::{Command, Stdio},
};

use waldo_verification::{DisplayRoute, FixtureToolkit};

use crate::toolkit_preflight::ToolkitPreflight;

#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureSpec {
    pub name: &'static str,
    pub toolkit: FixtureToolkit,
    pub display_route: DisplayRoute,
    pub script: &'static str,
    pub runner: Runner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Runner {
    Python,
    Electron,
}

pub(crate) const SPECS: [FixtureSpec; 4] = [
    FixtureSpec {
        name: "gtk",
        toolkit: FixtureToolkit::Gtk,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_gtk.py",
        runner: Runner::Python,
    },
    FixtureSpec {
        name: "qt",
        toolkit: FixtureToolkit::Qt,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_qt.py",
        runner: Runner::Python,
    },
    FixtureSpec {
        name: "electron",
        toolkit: FixtureToolkit::Electron,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_electron.js",
        runner: Runner::Electron,
    },
    FixtureSpec {
        name: "xwayland",
        toolkit: FixtureToolkit::Xwayland,
        display_route: DisplayRoute::Xwayland,
        script: "interaction_qt.py",
        runner: Runner::Python,
    },
];

/// The backend a fixture reports when it really runs on its display route:
/// GTK's display class, Qt's platform name, and Electron's Ozone platform.
pub(crate) fn expected_backend(toolkit: FixtureToolkit, route: DisplayRoute) -> &'static str {
    match (toolkit, route) {
        (_, DisplayRoute::Xwayland) => "xcb",
        (FixtureToolkit::Gtk, _) => "GdkWaylandDisplay",
        (FixtureToolkit::Electron, _) => "ozone-wayland",
        (FixtureToolkit::Qt | FixtureToolkit::Xwayland, _) => "wayland",
    }
}

impl FixtureSpec {
    /// The command running this fixture on its runner and display route,
    /// with `seed`; each phase adds its own state and control arguments.
    pub(crate) fn command(
        self,
        fixture_directory: &Path,
        python: &Path,
        electron: &Path,
        seed: u64,
    ) -> Command {
        let mut command = match self.runner {
            Runner::Python => {
                let mut command = Command::new(python);
                command.env("PYTHONDONTWRITEBYTECODE", "1");
                command
            }
            Runner::Electron => Command::new(electron),
        };
        command
            .arg(fixture_directory.join(self.script))
            .args(["--seed", &seed.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if self.toolkit == FixtureToolkit::Gtk {
            command.env("GDK_BACKEND", "wayland");
        }
        if self.display_route == DisplayRoute::Xwayland {
            command.args(["--display-route", "xwayland"]);
        }
        command
    }
}

impl ToolkitPreflight {
    /// Whether this fixture reached every oracle on its requested route,
    /// which admits it to the scenarios and the final campaign alike.
    pub(crate) fn admitted(&self) -> bool {
        self.descriptor_valid
            && self.final_consumer_ready
            && self.event_log_valid
            && self.compositor_window_seen
            && self.cleanup_complete
            && self.runtime_backend.as_deref()
                == Some(expected_backend(self.toolkit, self.display_route))
    }
}

#[cfg(test)]
mod tests {
    use waldo_verification::{DisplayRoute, FixtureToolkit};

    use crate::toolkit_preflight::ToolkitPreflight;

    fn preflight(toolkit: FixtureToolkit, backend: &str) -> ToolkitPreflight {
        ToolkitPreflight {
            toolkit,
            display_route: DisplayRoute::NativeWayland,
            descriptor_valid: true,
            runtime_backend: Some(backend.to_owned()),
            graphical_launch_attempted: true,
            final_consumer_ready: true,
            event_log_valid: true,
            event_count: 1,
            event_log_sha256: None,
            compositor_window_seen: true,
            x11_root_window_seen: None,
            cleanup_complete: true,
            limitation: None,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn a_native_fixture_running_through_xwayland_is_not_admitted() {
        assert!(preflight(FixtureToolkit::Qt, "wayland").admitted());
        assert!(!preflight(FixtureToolkit::Qt, "xcb").admitted());
        assert!(preflight(FixtureToolkit::Gtk, "GdkWaylandDisplay").admitted());
        assert!(!preflight(FixtureToolkit::Gtk, "GdkX11Display").admitted());
    }
}
