//! Tests for the overview (`src/ecs/overview.rs`).

use bevy::ecs::entity::Entity;
use bevy::ecs::system::SystemState;
use bevy::ecs::world::World;
use bevy::math::{IRect, IVec2};
use objc2_core_foundation::CGPoint;

use crate::assert_focused;
use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::overview::{
    KeyAction, Overview, OverviewConfig, OverviewLayout, OverviewPhase, OverviewTile,
    initial_selection, key_action, move_selection, project, step_progress,
};
use crate::ecs::params::FrameActivity;
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
            // No `animation_speed` configured: the zoom is instant.
            let overview = world.get_resource::<Overview>().expect("overview open");
            assert_eq!(overview.phase, OverviewPhase::Open);
            assert!((overview.progress - 1.0).abs() < f32::EPSILON);
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

// ── Projection (pure) ──────────────────────────────────────────────────────

const VIEWPORT: IRect = IRect {
    min: IVec2::new(0, 0),
    max: IVec2::new(1000, 600),
};
const NO_GAPS: OverviewConfig = OverviewConfig {
    row_gap: 0,
    label_height: 0,
};

fn entities(count: usize) -> Vec<Entity> {
    World::new().spawn_batch(vec![(); count]).collect()
}

/// Every window 400x600, sitting at the origin.
#[allow(
    clippy::unnecessary_wraps,
    reason = "shaped like `project`'s `frame_of`"
)]
fn unit_frame(_: Entity) -> Option<IRect> {
    Some(IRect::new(0, 0, 400, 600))
}

fn tiles_of(layout: &OverviewLayout, row: usize) -> Vec<(Entity, IRect)> {
    layout.rows[row]
        .tiles
        .iter()
        .map(|tile| (tile.entity, tile.target))
        .collect()
}

#[test]
fn test_overview_projects_stack_column_in_order() {
    let wins = entities(4);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &wins {
        strip.append(entity);
    }
    strip
        .stack(wins[2])
        .expect("stack onto the column to the left");

    let layout = project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &unit_frame);
    let tiles = tiles_of(&layout, 0);

    assert_eq!(tiles.len(), 4);
    assert_eq!(
        tiles.iter().map(|(entity, _)| *entity).collect::<Vec<_>>(),
        wins
    );
    let (a, b, c, d) = (tiles[0].1, tiles[1].1, tiles[2].1, tiles[3].1);
    assert!(
        a.max.x <= b.min.x && b.max.x <= d.min.x,
        "columns left to right"
    );
    assert_eq!(
        (b.min.x, b.max.x),
        (c.min.x, c.max.x),
        "stack shares x-range"
    );
    assert!(b.max.y <= c.min.y, "stack top above bottom");
    assert!(
        tiles
            .iter()
            .all(|(_, rect)| VIEWPORT.contains(rect.min) && VIEWPORT.contains(rect.max)),
        "tiles fit the viewport"
    );
}

#[test]
fn test_overview_projects_rows_in_virtual_order_without_overlap() {
    let wins = entities(3);
    let strips = [2, 0, 1].map(|index| {
        let mut strip = LayoutStrip::new(1, index);
        strip.append(wins[index as usize]);
        strip
    });
    let rows = [
        (wins[0], &strips[0], false),
        (wins[1], &strips[1], true),
        (wins[2], &strips[2], false),
    ];
    let config = OverviewConfig {
        row_gap: 24,
        label_height: 20,
    };

    let layout = project(&rows, VIEWPORT, config, &unit_frame);

    assert_eq!(layout.rows.len(), 3);
    assert_eq!(
        layout
            .rows
            .iter()
            .map(|row| row.virtual_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    for pair in layout.rows.windows(2) {
        assert!(pair[0].band.max.y < pair[1].band.min.y, "bands overlap");
    }
    for row in &layout.rows {
        let tile = row.tiles[0].target;
        assert!(tile.min.y >= row.band.min.y + config.label_height);
        assert!(tile.max.y <= row.band.max.y);
    }
}

#[test]
fn test_overview_parked_row_projects_like_active_row() {
    let wins = entities(3);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &wins {
        strip.append(entity);
    }
    let parked_corner = VIEWPORT.max - PARKED_STRIP_SLIVER;
    let parked = |entity: Entity| {
        let offset = i32::try_from(wins.iter().position(|&other| other == entity)?).ok()? * 400;
        let min = parked_corner + IVec2::new(offset, 0);
        Some(IRect::from_corners(min, min + IVec2::new(400, 600)))
    };

    let visible = project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &unit_frame);
    let parked_layout = project(&[(wins[0], &strip, false)], VIEWPORT, NO_GAPS, &parked);

    assert_eq!(tiles_of(&visible, 0), tiles_of(&parked_layout, 0));
    assert_eq!(
        parked_layout.rows[0].tiles[0].origin.min, parked_corner,
        "origin is the real, parked frame"
    );
}

#[test]
fn test_overview_scrolled_column_projects_at_strip_position() {
    let wins = entities(3);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &wins {
        strip.append(entity);
    }
    // The third column is scrolled off the right edge, down to a 5px sliver.
    let slivered = |entity: Entity| {
        if entity == wins[2] {
            let x = VIEWPORT.max.x - 5;
            Some(IRect::new(x, 0, x + 400, 600))
        } else {
            unit_frame(entity)
        }
    };

    let layout = project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &slivered);
    let tiles = tiles_of(&layout, 0);

    assert_eq!(
        tiles,
        tiles_of(
            &project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &unit_frame),
            0
        )
    );
    assert_eq!(
        tiles[2].1.min.x, tiles[1].1.max.x,
        "adjacent to its neighbour"
    );
}

#[test]
fn test_overview_empty_strip_yields_empty_band() {
    let wins = entities(1);
    let strip = LayoutStrip::new(1, 0);

    let layout = project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &unit_frame);

    assert_eq!(layout.rows.len(), 1);
    assert!(layout.rows[0].tiles.is_empty());
    assert!(layout.rows[0].band.width() > 0 && layout.rows[0].band.height() > 0);
    assert!(project(&[], VIEWPORT, NO_GAPS, &unit_frame).rows.is_empty());
}

#[test]
fn test_overview_fullscreen_and_tabs_are_one_tile() {
    let wins = entities(4);
    let fullscreen = LayoutStrip::fullscreen(1, wins[0]);
    let mut tabbed = LayoutStrip::new(1, 1);
    tabbed.append_tab_group(&wins[1..4]);

    let layout = project(
        &[(wins[0], &fullscreen, true), (wins[1], &tabbed, false)],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

    assert_eq!(layout.rows[0].tiles.len(), 1);
    assert_eq!(layout.rows[0].tiles[0].tab_count, 1);
    assert_eq!(layout.rows[1].tiles.len(), 1);
    assert_eq!(layout.rows[1].tiles[0].entity, wins[1]);
    assert_eq!(layout.rows[1].tiles[0].tab_count, 3);
}

#[test]
fn test_overview_zero_width_frames_do_not_panic() {
    let wins = entities(2);
    let mut strip = LayoutStrip::new(1, 0);
    strip.append(wins[0]);
    strip.append(wins[1]);

    let layout = project(&[(wins[0], &strip, true)], VIEWPORT, NO_GAPS, &|_| {
        Some(IRect::new(0, 0, 0, 0))
    });

    assert_eq!(layout.rows.len(), 1);
}

#[test]
fn test_overview_tile_frame_at_endpoints() {
    let tile = OverviewTile {
        entity: entities(1)[0],
        target: IRect::new(10, 20, 110, 70),
        origin: IRect::new(-500, 900, 300, 1500),
        tab_count: 1,
    };
    assert_eq!(tile.frame_at(0.0), tile.origin);
    assert_eq!(tile.frame_at(1.0), tile.target);
    assert_eq!(tile.frame_at(0.5), IRect::new(-245, 460, 205, 785));
}

#[test]
fn test_overview_open_projects_active_row() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            let overview = world.get_resource::<Overview>().expect("overview open");
            let active = overview
                .layout
                .rows
                .iter()
                .find(|row| row.is_active)
                .expect("active row projected");
            assert_eq!(active.tiles.len(), 2);
        })
        .run(vec![toggle()]);
}

// ── Animation ──────────────────────────────────────────────────────────────

/// Steps `progress` towards `goal` until it lands, asserting every step moves
/// the right way. Returns the number of steps taken.
#[allow(clippy::float_cmp, reason = "landing on exactly the goal is the point")]
fn animate_to(mut progress: f32, goal: f32) -> usize {
    for step in 1..=1000 {
        let next = step_progress(progress, goal, 0.2);
        assert!(
            (goal - next).abs() < (goal - progress).abs(),
            "step {step} did not approach {goal}: {progress} -> {next}"
        );
        progress = next;
        if progress == goal {
            return step;
        }
    }
    panic!("never reached {goal}");
}

#[test]
fn test_overview_progress_lands_exactly_on_its_goal() {
    assert!(animate_to(0.0, 1.0) < 100);
    assert!(animate_to(1.0, 0.0) < 100);
    assert!(
        (step_progress(0.3, 1.0, 1.0) - 1.0).abs() < f32::EPSILON,
        "t = 1 snaps"
    );
}

#[test]
fn test_overview_mid_frame_only_while_animating() {
    let mid_frame = |phase: Option<OverviewPhase>| {
        let mut world = World::new();
        if let Some(phase) = phase {
            world.insert_resource(Overview {
                phase,
                progress: 0.5,
                selected: None,
                hovered: None,
                layout: OverviewLayout::default(),
            });
        }
        let mut state = SystemState::<FrameActivity>::new(&mut world);
        state
            .get(&world)
            .expect("FrameActivity validates on a bare world")
            .mid_frame()
    };

    assert!(!mid_frame(None));
    assert!(mid_frame(Some(OverviewPhase::Opening)));
    assert!(mid_frame(Some(OverviewPhase::Closing { activate: None })));
    assert!(
        !mid_frame(Some(OverviewPhase::Open)),
        "a settled overview idles"
    );
}

// ── Key mapping (pure) ─────────────────────────────────────────────────────

/// A config from `init.lua` has no TOML bindings at all: every chord lives in
/// the Lua keybind set, so that is where the overview must look.
#[test]
fn test_overview_keys_follow_lua_binds() {
    const KEY_O: u8 = 31;
    const KEY_H: u8 = 4;
    const KEY_F: u8 = 3;
    let binds = [
        (KEY_O, Modifiers::ALT, 1, Some(Command::Overview)),
        (
            KEY_H,
            Modifiers::ALT,
            2,
            Some(Command::Window(Operation::Focus(Direction::West))),
        ),
        // A function handler: nothing to map.
        (KEY_F, Modifiers::ALT, 3, None),
    ];
    assert_eq!(
        key_action(KEY_O, Modifiers::ALT, None, &binds),
        Some(KeyAction::Dismiss)
    );
    assert_eq!(
        key_action(KEY_H, Modifiers::ALT, None, &binds),
        Some(KeyAction::Move(Direction::West))
    );
    assert_eq!(key_action(KEY_F, Modifiers::ALT, None, &binds), None);
    let virtual_south = [(
        KEY_H,
        Modifiers::ALT,
        4,
        Some(Command::Window(Operation::Virtual(Direction::South))),
    )];
    assert_eq!(
        key_action(KEY_H, Modifiers::ALT, None, &virtual_south),
        Some(KeyAction::Move(Direction::South))
    );
    assert_eq!(key_action(KEY_H, Modifiers::empty(), None, &binds), None);
}

// ── Selection and activation (harness) ─────────────────────────────────────

const KEY_RETURN: u8 = 36;
const KEY_DOWN: u8 = 125;
const KEY_RIGHT: u8 = 124;
const KEY_LEFT: u8 = 123;

fn key(keycode: u8) -> Event {
    Event::OverviewKey {
        keycode,
        modifiers: Modifiers::empty(),
    }
}

fn active_virtual_index(world: &mut World) -> u32 {
    let mut query =
        world.query_filtered::<&LayoutStrip, bevy::ecs::query::With<ActiveWorkspaceMarker>>();
    query
        .single(world)
        .expect("exactly one active strip")
        .virtual_index
}

/// Sends the focused window to VW1; focus moves to its neighbour on VW0.
fn send_window_down() -> Event {
    Event::Command {
        command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    }
}

/// The windows of the strip at `virtual_index`, in column order.
fn row_windows(world: &mut World, virtual_index: u32) -> Vec<Entity> {
    let mut query = world.query::<&LayoutStrip>();
    query
        .iter(world)
        .find(|strip| strip.virtual_index == virtual_index)
        .map(|strip| {
            strip
                .columns()
                .filter_map(crate::ecs::layout::Column::top)
                .collect()
        })
        .unwrap_or_default()
}

fn focused_entity(world: &mut World) -> Option<Entity> {
    let mut query =
        world.query_filtered::<Entity, bevy::ecs::query::With<crate::ecs::FocusedMarker>>();
    query.single(world).ok()
}

/// Sends windows 0 and 1 to VW1 (focus ends on window 2, alone on VW0), opens
/// the overview, drops the cursor into VW1, pushes it `sideways` to one end of
/// the row and commits. Activating either end of a two-window row means one of
/// the two runs picks a window other than the row's remembered focus, which
/// `show_active_workspace` would restore if it won the race.
fn activate_row_end(sideways: u8, pick: fn(&[Entity]) -> Entity) {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(6, move |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_eq!(active_virtual_index(world), 1, "switched to VW1");
            let expected = pick(&row_windows(world, 1));
            assert_eq!(focused_entity(world), Some(expected));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            send_window_down(),
            send_window_down(),
            toggle(),
            key(KEY_DOWN),
            key(sideways),
            key(KEY_RETURN),
        ]);
}

#[test]
fn test_overview_enter_activates_left_window_on_another_row() {
    activate_row_end(KEY_LEFT, |row| row[0]);
}

#[test]
fn test_overview_enter_activates_right_window_on_another_row() {
    activate_row_end(KEY_RIGHT, |row| row[row.len() - 1]);
}

// ── Selection (pure) ───────────────────────────────────────────────────────

/// Projects `strips` as rows, row 0 active, pairing each with its own entity.
fn rows_of(strips: &[LayoutStrip]) -> (OverviewLayout, Vec<(Entity, &LayoutStrip)>) {
    let ids = entities(strips.len());
    let paired = ids.iter().copied().zip(strips).collect::<Vec<_>>();
    let rows = paired
        .iter()
        .enumerate()
        .map(|(index, (entity, strip))| (*entity, *strip, index == 0))
        .collect::<Vec<_>>();
    (project(&rows, VIEWPORT, NO_GAPS, &unit_frame), paired)
}

fn strip_of(virtual_index: u32, columns: &[&[Entity]]) -> LayoutStrip {
    let mut strip = LayoutStrip::new(1, virtual_index);
    for column in columns {
        strip.append(column[0]);
        for &below in &column[1..] {
            strip.append(below);
            strip
                .stack(below)
                .expect("stack onto the column to the left");
        }
    }
    strip
}

#[test]
fn test_overview_cursor_east_from_last_column_stays_put() {
    let wins = entities(2);
    let strips = [strip_of(0, &[&[wins[0]], &[wins[1]]])];
    let (layout, strips) = rows_of(&strips);

    assert_eq!(
        move_selection(&layout, &strips, wins[1], &Direction::East),
        wins[1]
    );
    assert_eq!(
        move_selection(&layout, &strips, wins[0], &Direction::East),
        wins[1]
    );
}

#[test]
fn test_overview_cursor_south_mid_stack_moves_down_the_column() {
    let wins = entities(4);
    let strips = [strip_of(0, &[&[wins[0]], &[wins[1], wins[2], wins[3]]])];
    let (layout, strips) = rows_of(&strips);

    assert_eq!(
        move_selection(&layout, &strips, wins[1], &Direction::South),
        wins[2]
    );
    assert_eq!(
        move_selection(&layout, &strips, wins[2], &Direction::North),
        wins[1]
    );
}

#[test]
fn test_overview_cursor_south_at_stack_bottom_falls_to_nearest_column_below() {
    let wins = entities(5);
    let strips = [
        strip_of(0, &[&[wins[0]], &[wins[1], wins[2]]]),
        strip_of(1, &[&[wins[3]], &[wins[4]]]),
    ];
    let (layout, strips) = rows_of(&strips);

    assert_eq!(
        move_selection(&layout, &strips, wins[2], &Direction::South),
        wins[4],
        "right-hand column lands on the right-hand column below"
    );
    assert_eq!(
        move_selection(&layout, &strips, wins[0], &Direction::South),
        wins[3]
    );
}

#[test]
fn test_overview_cursor_south_on_bottom_row_stays_put() {
    let wins = entities(2);
    let strips = [strip_of(0, &[&[wins[0]]]), strip_of(1, &[&[wins[1]]])];
    let (layout, strips) = rows_of(&strips);

    assert_eq!(
        move_selection(&layout, &strips, wins[1], &Direction::South),
        wins[1]
    );
    assert_eq!(
        move_selection(&layout, &strips, wins[0], &Direction::North),
        wins[0],
        "no wrap at the top either"
    );
}

#[test]
fn test_overview_cursor_on_single_column_falls_through_immediately() {
    let wins = entities(2);
    let strips = [
        strip_of(0, &[&[wins[0]]]),
        LayoutStrip::new(1, 1),
        strip_of(2, &[&[wins[1]]]),
    ];
    let (layout, strips) = rows_of(&strips);

    assert_eq!(
        move_selection(&layout, &strips, wins[0], &Direction::South),
        wins[1],
        "skips the empty row"
    );
    assert_eq!(
        move_selection(&layout, &strips, wins[1], &Direction::North),
        wins[0]
    );
}

#[test]
fn test_overview_initial_selection_prefers_focus_then_active_row() {
    let wins = entities(3);
    let strips = [
        strip_of(0, &[&[wins[0]], &[wins[1]]]),
        strip_of(1, &[&[wins[2]]]),
    ];
    let (layout, _) = rows_of(&strips);

    assert_eq!(initial_selection(&layout, Some(wins[2])), Some(wins[2]));
    assert_eq!(initial_selection(&layout, None), Some(wins[0]));
    assert_eq!(
        initial_selection(&OverviewLayout::default(), Some(wins[2])),
        None
    );
}

// ── Selection and activation (more harness) ────────────────────────────────

fn strip_snapshot(world: &mut World) -> Vec<String> {
    let mut query = world.query::<(&LayoutStrip, bevy::ecs::query::Has<ActiveWorkspaceMarker>)>();
    let mut strips = query
        .iter(world)
        .map(|(strip, active)| format!("{} {active} {strip}", strip.virtual_index))
        .collect::<Vec<_>>();
    strips.sort();
    strips
}

#[test]
fn test_overview_escape_leaves_focus_and_layout_untouched() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let before = Rc::new(RefCell::new(Vec::new()));
    let recorded = before.clone();
    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, move |world, _state| {
            *recorded.borrow_mut() = strip_snapshot(world);
            assert_focused!(world, 0);
        })
        .on_iteration(3, move |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_focused!(world, 0);
            assert_eq!(strip_snapshot(world), *before.borrow());
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            key(KEY_RIGHT),
            key(KEY_ESCAPE),
        ]);
}

#[test]
fn test_overview_enter_in_active_row_moves_focus_without_switching() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(3, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_eq!(active_virtual_index(world), 0);
            assert_focused!(world, 1);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            key(KEY_RIGHT),
            key(KEY_RETURN),
        ]);
}

#[test]
fn test_overview_destroyed_selection_moves_to_a_live_tile() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, state| {
            let focused = find_window_entity(0, world);
            let overview = world.get_resource::<Overview>().expect("overview open");
            assert_eq!(overview.selected, Some(focused));
            state.os_close_window(0);
        })
        .on_iteration(2, |world, _state| {
            let overview = world.get_resource::<Overview>().expect("still open");
            let selected = overview.selected.expect("a selection");
            assert!(
                overview.layout.find(selected).is_some(),
                "selection is a live tile"
            );
            let tiles = overview
                .layout
                .rows
                .iter()
                .map(|row| row.tiles.len())
                .sum::<usize>();
            assert_eq!(tiles, 2, "the closed window's tile is gone");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            Event::MenuOpened { window_id: 1 },
        ]);
}

#[test]
fn test_overview_with_no_windows_opens_and_closes() {
    TestHarness::new()
        .on_iteration(0, |world, _state| {
            let overview = world.get_resource::<Overview>().expect("overview open");
            assert_eq!(overview.selected, None);
            assert!(overview.layout.rows.iter().all(|row| row.tiles.is_empty()));
        })
        .on_iteration(1, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none());
        })
        .run(vec![toggle(), key(KEY_RETURN)]);
}

#[test]
fn test_overview_click_activates_tile_and_miss_closes() {
    let tile_centre = |world: &mut World, id| {
        let entity = find_window_entity(id, world);
        let overview = world.get_resource::<Overview>().expect("overview open");
        let (_, tile) = overview.layout.find(entity).expect("tile");
        let centre = tile.target.center();
        CGPoint::new(f64::from(centre.x), f64::from(centre.y))
    };
    let click = |point| Event::MouseDown {
        point,
        modifiers: Modifiers::empty(),
    };

    // Window 1's tile sits at a fixed spot once the overview has settled.
    let mut harness = TestHarness::new().with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }, toggle()]);
    let point = tile_centre(harness.app.world_mut(), 1);
    harness
        .on_iteration(0, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_focused!(world, 1);
        })
        .on_iteration(2, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "a miss closes");
            assert_focused!(world, 1);
        })
        .run(vec![
            click(point),
            toggle(),
            click(CGPoint::new(-1000.0, -1000.0)),
        ]);
}

#[test]
fn test_overview_refuses_to_open_during_mission_control() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "refused");
        })
        .on_iteration(3, |world, _state| {
            assert!(
                world.get_resource::<Overview>().is_some(),
                "opens after exit"
            );
        })
        .on_iteration(4, |world, _state| {
            assert!(
                world.get_resource::<Overview>().is_none(),
                "Mission Control closes an open overview"
            );
        })
        .run(vec![
            Event::MissionControlShowAllWindows,
            toggle(),
            Event::MissionControlExit,
            toggle(),
            Event::MissionControlShowAllWindows,
        ]);
}

// ── Thumbnails ─────────────────────────────────────────────────────────────

#[test]
fn test_overview_thumbnail_for_unknown_window_is_dropped() {
    let thumbnail = |window_id| Event::OverviewThumbnail {
        window_id,
        width: 2,
        height: 2,
        rgba: vec![0; 16],
    };
    TestHarness::new()
        .with_windows(2)
        .on_iteration(2, |world, _state| {
            let overview = world.get_resource::<Overview>().expect("still open");
            assert_eq!(overview.phase, OverviewPhase::Open);
            assert_eq!(overview.layout.rows[0].tiles.len(), 2);
        })
        .on_iteration(3, |world, _state| {
            assert!(
                world.get_resource::<Overview>().is_none(),
                "arrives after close"
            );
        })
        .run(vec![
            toggle(),
            thumbnail(4242),
            thumbnail(0),
            toggle(),
            thumbnail(0),
        ]);
}
