//! The overview: a modal, zoomed-out map of every virtual workspace row on the
//! active display. Selection is deferred — nothing moves until the user commits.
//!
//! The `Overview` resource exists exactly while the overview is on screen, and
//! every system here is gated on it so nothing is scheduled while it is shut.

use bevy::app::{App, Plugin, PreUpdate};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Res};
use tracing::{Level, instrument};

use crate::commands::Command;
use crate::config::Config;
use crate::events::Event;
use crate::platform::input::set_overview_active;

/// Escape. Always closes, so a swallowed keyboard can never get stuck.
const KEY_ESCAPE: u8 = 53;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverviewPhase {
    /// Zooming out, `progress` rising towards 1.
    Opening,
    /// Settled and static.
    Open,
    /// Zooming back in, `progress` falling towards 0. Carries the window to
    /// activate once the animation finishes, if any.
    Closing { activate: Option<Entity> },
}

#[derive(Debug, Resource)]
pub struct Overview {
    pub phase: OverviewPhase,
    /// 0.0 = windows at their real on-screen frames, 1.0 = fully zoomed out.
    pub progress: f32,
    pub selected: Option<Entity>,
}

impl Overview {
    fn opening() -> Self {
        Self {
            phase: OverviewPhase::Opening,
            progress: 0.0,
            selected: None,
        }
    }
}

pub struct OverviewPlugin;

impl Plugin for OverviewPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, overview_toggle);
    }
}

/// Opens the overview on `Command::Overview` and closes it on a second one.
///
/// While open the event tap swallows the `overview` chord too, so it arrives as
/// an [`Event::OverviewKey`]; that — or Escape — closes it as well.
#[instrument(level = Level::DEBUG, skip_all)]
fn overview_toggle(
    mut messages: MessageReader<Event>,
    overview: Option<Res<Overview>>,
    config: Option<Res<Config>>,
    mut commands: Commands,
) {
    let is_overview_chord = |keycode: u8, modifiers| {
        config.as_ref().is_some_and(|config| {
            matches!(
                config.find_keybind(keycode, modifiers),
                Some(Command::Overview)
            )
        })
    };
    let toggled = messages.read().any(|event| match event {
        Event::Command {
            command: Command::Overview,
        } => true,
        Event::OverviewKey { keycode, modifiers } => {
            overview.is_some()
                && (*keycode == KEY_ESCAPE || is_overview_chord(*keycode, *modifiers))
        }
        _ => false,
    });
    if !toggled {
        return;
    }

    if overview.is_some() {
        commands.remove_resource::<Overview>();
        set_overview_active(false);
    } else {
        commands.insert_resource(Overview::opening());
        set_overview_active(true);
    }
}
