//! Tests for the overview (`src/ecs/overview.rs`).

use crate::commands::Command;
use crate::ecs::overview::{Overview, OverviewPhase};
use crate::events::Event;
use crate::platform::Modifiers;

use super::*;

const KEY_ESCAPE: u8 = 53;

fn toggle() -> Event {
    Event::Command {
        command: Command::Overview,
    }
}

#[test]
fn test_overview_command_toggles_resource() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            let overview = world.get_resource::<Overview>().expect("overview open");
            assert_eq!(overview.phase, OverviewPhase::Opening);
        })
        .on_iteration(1, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none());
        })
        .run(vec![toggle(), toggle()]);
}

#[test]
fn test_overview_escape_closes() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            assert!(world.get_resource::<Overview>().is_some());
        })
        .on_iteration(1, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none());
        })
        .run(vec![
            toggle(),
            Event::OverviewKey {
                keycode: KEY_ESCAPE,
                modifiers: Modifiers::empty(),
            },
        ]);
}

#[test]
fn test_overview_key_ignored_while_closed() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none());
        })
        .run(vec![Event::OverviewKey {
            keycode: KEY_ESCAPE,
            modifiers: Modifiers::empty(),
        }]);
}
