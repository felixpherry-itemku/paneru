//! Tests for `options.dynamic_workspaces` (niri-style dynamic virtual
//! workspaces): `maintain_dynamic_workspaces` and the spare-row caps in
//! `src/ecs/workspace.rs`. Verifiers are keyed by 0-based command position.

use bevy::prelude::*;

use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::overview::Overview;
use crate::ecs::workspace::VirtualMoveMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, FlashMessage, FocusedMarker, PreviousManagedStrip, Timeout,
};
use crate::events::Event;
use crate::manager::Window;
use crate::platform::WinID;

use super::*;

fn config() -> Config {
    (
        MainOptions {
            dynamic_workspaces: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into()
}

fn cmd(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// A command that changes nothing, to let queued OS events play out.
fn pump() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

/// `(virtual_index, columns, active)` for every row of `workspace_id`.
fn rows_of(world: &mut World, workspace_id: WorkspaceId) -> Vec<(u32, usize, bool)> {
    let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
    let mut rows = query
        .iter(world)
        .filter(|(strip, _)| strip.id() == workspace_id)
        .map(|(strip, active)| (strip.virtual_index, strip.len(), active))
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows
}

fn rows(world: &mut World) -> Vec<(u32, usize, bool)> {
    rows_of(world, TEST_WORKSPACE_ID)
}

/// The ids of the windows in row `virtual_index` of the test space.
fn row_windows(world: &mut World, virtual_index: u32) -> Vec<WinID> {
    let mut query = world.query::<&LayoutStrip>();
    let entities = query
        .iter(world)
        .find(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == virtual_index)
        .map(LayoutStrip::all_windows)
        .unwrap_or_default();
    entities
        .into_iter()
        .filter_map(|entity| world.get::<Window>(entity).map(|window| window.id()))
        .collect()
}

/// The text of the most recently spawned popup.
fn latest_flash(world: &mut World) -> Option<String> {
    let mut query = world.query::<(&FlashMessage, &Timeout)>();
    query
        .iter(world)
        .min_by_key(|(_, timeout)| timeout.timer.elapsed())
        .map(|(message, _)| message.0.clone())
}

/// Sends the focused window of a fresh test space to row `virtual_index`,
/// leaving the rest where they are.
fn send(virtual_index: u32) -> Event {
    cmd(Operation::VirtualMoveNumber(virtual_index, MoveFocus::Stay))
}

/// Three windows, laid out as `[0: 1 | 1: empty, shown | 2: 1 | 3: spare]`
/// once command 4 has run: two are sent to rows 1 and 2, row 1 is shown, and
/// its only window closes. Callers add verifiers from iteration 4 on.
fn shown_empty_middle_row() -> (TestHarness, Vec<Event>) {
    let harness = TestHarness::new()
        .with_config(config())
        .with_windows(3)
        .on_iteration(3, |world, state| {
            state.os_close_window(row_windows(world, 1)[0]);
        });
    let commands = vec![
        pump(),
        send(1),
        send(2),
        cmd(Operation::VirtualNumber(1)),
        pump(),
    ];
    (harness, commands)
}

/// T2: an empty row stays for as long as it is shown.
#[test]
fn test_shown_row_survives_losing_its_last_window() {
    let (harness, commands) = shown_empty_middle_row();
    harness
        .on_iteration(4, |world, _state| {
            assert_eq!(
                rows(world),
                [(0, 1, false), (1, 0, true), (2, 1, false), (3, 0, false)]
            );
        })
        .run(commands);
}

/// T1 + T5: switching away from a shown empty row removes it; the target is
/// renumbered and the popup already shows its new number.
#[test]
fn test_switch_past_shown_empty_row_removes_it() {
    let (harness, mut commands) = shown_empty_middle_row();
    commands.push(cmd(Operation::VirtualNumber(2)));
    harness
        .on_iteration(5, |world, _state| {
            assert_eq!(rows(world), [(0, 1, false), (1, 1, true), (2, 0, false)]);
            assert_eq!(latest_flash(world).as_deref(), Some("2"));
        })
        .run(commands);
}

/// T3: leaving an empty first row removes it too (row 1 is not special).
#[test]
fn test_leaving_empty_first_row_removes_it() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(1, |world, state| {
            state.os_close_window(row_windows(world, 0)[0]);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(rows(world), [(0, 0, true), (1, 1, false), (2, 0, false)]);
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 0, false)]);
            assert_eq!(latest_flash(world).as_deref(), Some("1"));
        })
        .run(vec![
            pump(),
            send(1),
            pump(),
            cmd(Operation::VirtualNumber(1)),
        ]);
}

/// T4: `alt-9` with one occupied row lands on the spare, creating nothing.
#[test]
fn test_numbered_switch_caps_at_spare() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            assert_eq!(rows(world), [(0, 2, true), (1, 0, false)]);
        })
        .on_iteration(1, |world, _state| {
            assert_eq!(rows(world), [(0, 2, false), (1, 0, true)]);
            assert_eq!(latest_flash(world).as_deref(), Some("2"));
        })
        .run(vec![pump(), cmd(Operation::VirtualNumber(8))]);
}

/// Edge case 7: the cap is the spare, not "last occupied + 1". From a shown
/// empty row just before the spare, `alt-9` goes to the spare, and the shown
/// row is removed behind it (niri does the same).
#[test]
fn test_cap_goes_to_spare_past_shown_empty_row() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(2, |world, state| {
            state.os_close_window(row_windows(world, 1)[0]);
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(rows(world), [(0, 1, false), (1, 0, true), (2, 0, false)]);
        })
        .on_iteration(4, |world, _state| {
            assert_eq!(rows(world), [(0, 1, false), (1, 0, true)]);
            assert_eq!(latest_flash(world).as_deref(), Some("2"));
        })
        .run(vec![
            pump(),
            send(1),
            cmd(Operation::VirtualNumber(1)),
            pump(),
            cmd(Operation::VirtualNumber(8)),
        ]);
}

/// T6: a hidden row emptied by a window closing is removed right away.
#[test]
fn test_hidden_row_emptied_is_removed() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(1, |world, state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 1, false), (2, 0, false)]);
            state.os_close_window(row_windows(world, 1)[0]);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 0, false)]);
        })
        .run(vec![pump(), send(1), pump()]);
}

/// T7: numbered moves are capped at the spare like switches; filling the
/// spare makes a new one.
#[test]
fn test_numbered_move_caps_at_spare() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 1, false), (2, 0, false)]);
        })
        .run(vec![pump(), send(8)]);
}

/// T8: moving south from the last occupied row goes into the spare.
#[test]
fn test_move_south_fills_spare() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_eq!(rows(world), [(0, 1, false), (1, 1, true), (2, 0, false)]);
            assert_eq!(latest_flash(world).as_deref(), Some("2"));
        })
        .run(vec![
            pump(),
            cmd(Operation::VirtualMove(Direction::South, MoveFocus::Follow)),
        ]);
}

/// T9: a minimized window remembers its row by number. When an earlier row
/// is removed, the number follows the renumbering, so restoring the window
/// puts it back beside its old neighbour instead of into the spare.
#[test]
fn test_minimized_window_returns_to_renumbered_row() {
    let previous_row = |world: &mut World| {
        let entity = find_window_entity(2, world);
        world
            .get::<PreviousManagedStrip>(entity)
            .map(|previous| previous.virtual_index)
    };
    TestHarness::new()
        .with_config(config())
        .with_windows(4)
        .on_iteration(0, |_world, state| state.focus_window(1))
        .on_iteration(2, |_world, state| state.focus_window(2))
        .on_iteration(4, |_world, state| state.focus_window(3))
        .on_iteration(6, |world, state| {
            assert_eq!(row_windows(world, 0), [0]);
            assert_eq!(row_windows(world, 1), [1]);
            assert_eq!(row_windows(world, 2), [2, 3]);
            state.os_minimize_window(2, true);
        })
        .on_iteration(7, move |world, state| {
            assert_eq!(previous_row(world), Some(2));
            state.os_close_window(1);
        })
        .on_iteration(8, move |world, state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 1, false), (2, 0, false)]);
            assert_eq!(previous_row(world), Some(1));
            state.os_minimize_window(2, false);
        })
        .on_iteration(9, |world, _state| {
            let mut windows = row_windows(world, 1);
            windows.sort_unstable();
            assert_eq!(windows, [2, 3]);
        })
        .run(vec![
            pump(),
            pump(),
            send(1),
            pump(),
            send(2),
            pump(),
            send(2),
            pump(),
            pump(),
            pump(),
        ]);
}

/// T10: protection is per space. A display that is not active still shows its
/// selected row, so that row survives even while empty.
#[test]
fn test_selected_row_on_inactive_display_is_kept() {
    let ext_frame = IRect::new(
        0,
        -EXT_DISPLAY_HEIGHT,
        TEST_WINDOW_WIDTH,
        -EXT_DISPLAY_HEIGHT + TEST_WINDOW_HEIGHT,
    );
    TestHarness::new()
        .with_config(config())
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| window.frame = ext_frame)
        .with_workspace_window(101, EXT_WORKSPACE_ID, |window| window.frame = ext_frame)
        .on_iteration(0, |_world, state| state.set_active_display(EXT_DISPLAY_ID))
        .on_iteration(1, |world, state| {
            assert_eq!(
                rows_of(world, EXT_WORKSPACE_ID),
                [(0, 2, true), (1, 0, false)]
            );
            state.focus_window(100);
        })
        .on_iteration(3, |_world, state| state.os_close_window(101))
        .on_iteration(4, |world, state| {
            assert_eq!(
                rows_of(world, EXT_WORKSPACE_ID),
                [(0, 0, true), (1, 1, false), (2, 0, false)]
            );
            state.set_active_display(TEST_DISPLAY_ID);
        })
        .on_iteration(5, |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 0, false)]);
            assert_eq!(
                rows_of(world, EXT_WORKSPACE_ID),
                [(0, 0, false), (1, 1, false), (2, 0, false)]
            );
        })
        .run(vec![
            pump(),
            Event::DisplayChanged,
            pump(),
            send(1),
            pump(),
            Event::DisplayChanged,
        ]);
}

/// T11: a native fullscreen space holds its one window and never gets a
/// spare row. The pattern follows the fullscreen tests in `interaction.rs`.
#[test]
fn test_fullscreen_space_gets_no_spare() {
    const FULLSCREEN_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 100;

    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(0, |world, state| {
            let focused = world
                .query_filtered::<Entity, With<FocusedMarker>>()
                .iter(world)
                .collect::<Vec<_>>();
            for entity in focused {
                world.entity_mut(entity).remove::<FocusedMarker>();
            }
            state.update_window(0, |window| {
                window.workspace_id = FULLSCREEN_WORKSPACE_ID;
                window.is_full_screen = true;
            });
            state.activate_workspace(TEST_DISPLAY_ID, FULLSCREEN_WORKSPACE_ID, true);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(rows_of(world, FULLSCREEN_WORKSPACE_ID), [(0, 1, true)]);
            assert_eq!(rows(world), [(0, 1, false), (1, 0, false)]);
        })
        .run(vec![pump(), Event::SpaceChanged, pump()]);
}

/// T12: `default_workspaces` pre-creates nothing with dynamic workspaces on.
#[test]
fn test_default_workspaces_ignored() {
    let config = Config::try_from(
        "default_workspaces = 3\n[options]\ndynamic_workspaces = true\n[bindings]\n",
    )
    .expect("config parses");
    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(0, |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 0, false)]);
        })
        .run(vec![pump()]);
}

/// `(virtual_index, tiles)` for every row the open overview projects.
fn overview_rows(world: &mut World) -> Vec<(u32, usize)> {
    let overview = world.get_resource::<Overview>().expect("overview open");
    overview
        .layout
        .rows
        .iter()
        .map(|row| (row.virtual_index, row.tiles.len()))
        .collect()
}

fn toggle_overview() -> Event {
    Event::Command {
        command: Command::Overview,
    }
}

/// T13a: the overview shows the spare as the last, empty row.
#[test]
fn test_overview_shows_spare_row() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_eq!(overview_rows(world), [(0, 2), (1, 0)]);
        })
        .run(vec![pump(), toggle_overview()]);
}

/// T13b: a row removed while the overview is open drops out of it, and the
/// rows after it are relabelled.
#[test]
fn test_overview_drops_removed_row() {
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(2, |world, state| {
            assert_eq!(overview_rows(world), [(0, 1), (1, 1), (2, 0)]);
            state.os_close_window(row_windows(world, 1)[0]);
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(overview_rows(world), [(0, 1), (1, 0)]);
        })
        .run(vec![pump(), send(1), toggle_overview(), pump()]);
}

/// T15: Lua's `MoveToWorkspace` inserts the move marker directly, skipping the
/// keybind caps; the move executor caps it at the spare all the same. The
/// window must land in the spare itself: an overshooting row would also be
/// compacted to index 1 by the invariant, but as a new strip.
#[test]
fn test_direct_move_marker_caps_at_spare() {
    let spare = std::rc::Rc::new(std::cell::Cell::new(None));
    let spare_before = spare.clone();
    TestHarness::new()
        .with_config(config())
        .with_windows(2)
        .on_iteration(0, move |world, _state| {
            let mut strips = world.query::<(Entity, &LayoutStrip)>();
            spare_before.set(
                strips
                    .iter(world)
                    .find_map(|(entity, strip)| (strip.virtual_index == 1).then_some(entity)),
            );
            let focused = world
                .query_filtered::<Entity, With<FocusedMarker>>()
                .single(world)
                .expect("a focused window");
            world.entity_mut(focused).insert(VirtualMoveMarker {
                target_virtual_index: 8,
                move_focus: MoveFocus::Stay,
            });
        })
        .on_iteration(1, move |world, _state| {
            assert_eq!(rows(world), [(0, 1, true), (1, 1, false), (2, 0, false)]);
            let mut strips = world.query::<(Entity, &LayoutStrip)>();
            let filled = strips
                .iter(world)
                .find_map(|(entity, strip)| (strip.virtual_index == 1).then_some(entity));
            assert!(spare.get().is_some());
            assert_eq!(filled, spare.get());
        })
        .run(vec![pump(), pump()]);
}
