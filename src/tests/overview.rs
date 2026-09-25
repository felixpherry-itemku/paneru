//! Tests for the overview (`src/ecs/overview.rs`).

use std::collections::HashMap;

use bevy::ecs::entity::Entity;
use bevy::ecs::system::SystemState;
use bevy::ecs::world::World;
use bevy::math::{IRect, IVec2};
use objc2_core_foundation::CGPoint;

use crate::assert_focused;
use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::overview::{
    KeyAction, Overview, OverviewConfig, OverviewLayout, OverviewPhase, OverviewTile, key_action,
    project, step_progress,
};
use crate::ecs::params::FrameActivity;
use crate::ecs::{ActiveDisplayMarker, ActiveWorkspaceMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::Display;
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
    zoom: 0.5,
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

/// A strip at `virtual_index` with one column per window.
fn strip_of(virtual_index: u32, wins: &[Entity]) -> LayoutStrip {
    let mut strip = LayoutStrip::new(1, virtual_index);
    for &entity in wins {
        strip.append(entity);
    }
    strip
}

/// Each tile's target x range in `row`.
fn x_ranges(layout: &OverviewLayout, row: usize) -> Vec<(i32, i32)> {
    layout.rows[row]
        .tiles
        .iter()
        .map(|tile| (tile.target.min.x, tile.target.max.x))
        .collect()
}

#[test]
fn test_overview_centres_the_centre_column() {
    let wins = entities(3);
    let strip = strip_of(0, &wins);

    let layout = project(
        &[(wins[0], &strip, true, Some(wins[0]))],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

    assert_eq!(
        x_ranges(&layout, 0),
        vec![(400, 600), (600, 800), (800, 1000)]
    );
    for tile in &layout.rows[0].tiles {
        assert_eq!((tile.target.min.y, tile.target.max.y), (150, 450));
    }
}

#[test]
fn test_overview_centre_on_middle_column_is_symmetric() {
    let wins = entities(3);
    let strip = strip_of(0, &wins);

    let layout = project(
        &[(wins[0], &strip, true, Some(wins[1]))],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

    assert_eq!(
        x_ranges(&layout, 0),
        vec![(200, 400), (400, 600), (600, 800)]
    );
}

#[test]
fn test_overview_rows_share_one_zoom() {
    let wins = entities(5);
    let one = strip_of(0, &wins[..1]);
    let four = strip_of(1, &wins[1..]);

    let layout = project(
        &[(wins[0], &one, true, None), (wins[1], &four, false, None)],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

    let sizes = layout
        .rows
        .iter()
        .flat_map(|row| &row.tiles)
        .map(|tile| tile.target.size())
        .collect::<Vec<_>>();
    assert_eq!(sizes, vec![IVec2::new(200, 300); 5]);
}

#[test]
fn test_overview_parked_row_origin_is_stacked_offscreen() {
    let wins = entities(2);
    let active = strip_of(0, &wins[..1]);
    let parked = strip_of(1, &wins[1..]);
    let config = OverviewConfig {
        row_gap: 24,
        zoom: 0.5,
    };

    let layout = project(
        &[
            (wins[0], &active, true, None),
            (wins[1], &parked, false, None),
        ],
        VIEWPORT,
        config,
        &unit_frame,
    );

    assert_eq!(
        layout.rows[0].tiles[0].origin,
        IRect::new(0, 0, 400, 600),
        "the active row starts at its real frames"
    );
    let origin = layout.rows[1].tiles[0].origin;
    assert_eq!(origin.min.y, VIEWPORT.min.y + 600 + config.row_gap);
    assert_eq!(origin.width(), 400, "full size");
}

#[test]
fn test_overview_missing_centre_falls_back_to_first_column() {
    let wins = entities(4);
    let strip = strip_of(0, &wins[..3]);
    let first_centred = vec![(400, 600), (600, 800), (800, 1000)];

    for centre in [None, Some(wins[3])] {
        let layout = project(
            &[(wins[0], &strip, true, centre)],
            VIEWPORT,
            NO_GAPS,
            &unit_frame,
        );
        assert_eq!(x_ranges(&layout, 0), first_centred, "centre {centre:?}");
    }
}

#[test]
fn test_overview_tab_member_centres_its_group() {
    let wins = entities(5);
    let mut strip = LayoutStrip::new(1, 0);
    strip.append(wins[0]);
    strip.append_tab_group(&wins[1..4]);
    strip.append(wins[4]);

    let layout = project(
        &[(wins[0], &strip, true, Some(wins[2]))],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

    let (_, group) = layout.find(wins[1]).expect("group tile");
    assert_eq!(group.tab_count, 3);
    assert_eq!((group.target.min.x, group.target.max.x), (400, 600));
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

    let layout = project(
        &[(wins[0], &strip, true, None)],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );
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
}

#[test]
fn test_overview_projects_rows_in_virtual_order_without_overlap() {
    let wins = entities(3);
    let strips = [2, 0, 1].map(|index| {
        let mut strip = LayoutStrip::new(1, index);
        strip.append(wins[index as usize]);
        strip
    });
    // Given out of order; the active row is VW1, the middle one once sorted.
    let rows = [
        (wins[0], &strips[0], false, None),
        (wins[1], &strips[1], false, None),
        (wins[2], &strips[2], true, None),
    ];
    let config = OverviewConfig {
        row_gap: 24,
        zoom: 0.5,
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
    assert!(layout.rows[1].is_active);
    assert_eq!(layout.rows[1].band.center().y, VIEWPORT.center().y);
    for pair in layout.rows.windows(2) {
        assert!(pair[0].band.max.y < pair[1].band.min.y, "bands overlap");
        assert_eq!(
            pair[1].band.min.y - pair[0].band.min.y,
            300 + config.row_gap
        );
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

    let visible = project(
        &[(wins[0], &strip, true, Some(wins[1]))],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );
    let parked_layout = project(
        &[(wins[0], &strip, false, Some(wins[1]))],
        VIEWPORT,
        NO_GAPS,
        &parked,
    );

    assert_eq!(tiles_of(&visible, 0), tiles_of(&parked_layout, 0));
    // The lone row is the vertical anchor (k = 0), so at full size it fills the
    // viewport from the top, centred on window 1 — not the parked corner.
    let origins = parked_layout.rows[0]
        .tiles
        .iter()
        .map(|tile| tile.origin)
        .collect::<Vec<_>>();
    assert_eq!(
        origins,
        vec![
            IRect::new(-100, 0, 300, 600),
            IRect::new(300, 0, 700, 600),
            IRect::new(700, 0, 1100, 600),
        ]
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

    let layout = project(
        &[(wins[0], &strip, true, None)],
        VIEWPORT,
        NO_GAPS,
        &slivered,
    );
    let tiles = tiles_of(&layout, 0);

    assert_eq!(
        tiles,
        tiles_of(
            &project(
                &[(wins[0], &strip, true, None)],
                VIEWPORT,
                NO_GAPS,
                &unit_frame
            ),
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

    let layout = project(
        &[(wins[0], &strip, true, None)],
        VIEWPORT,
        NO_GAPS,
        &unit_frame,
    );

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
        &[
            (wins[0], &fullscreen, true, None),
            (wins[1], &tabbed, false, None),
        ],
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

    let layout = project(&[(wins[0], &strip, true, None)], VIEWPORT, NO_GAPS, &|_| {
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
                display: 0,
                reproject: false,
                centres: HashMap::new(),
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

#[test]
fn test_overview_key_action_maps_bare_keys_only() {
    const KEY_KEYPAD_ENTER: u8 = 76;
    const KEY_LEFT: u8 = 123;
    const KEY_UP: u8 = 126;
    let bare = Modifiers::empty();
    for keycode in [KEY_ESCAPE, KEY_RETURN, KEY_KEYPAD_ENTER] {
        assert_eq!(key_action(keycode, bare), Some(KeyAction::Close));
    }
    for (keycode, direction) in [
        (KEY_LEFT, Direction::West),
        (KEY_RIGHT, Direction::East),
        (KEY_DOWN, Direction::South),
        (KEY_UP, Direction::North),
    ] {
        assert_eq!(key_action(keycode, bare), Some(KeyAction::Move(direction)));
    }
    assert_eq!(key_action(KEY_RETURN, Modifiers::LALT), None, "alt+Return");
    assert_eq!(key_action(KEY_LEFT, Modifiers::LCMD), None, "cmd+Left");
    assert_eq!(
        key_action(KEY_LEFT, Modifiers::LSHIFT),
        Some(KeyAction::Move(Direction::West)),
        "shift is not a chord"
    );
    assert_eq!(
        key_action(KEY_LEFT, Modifiers::FN),
        Some(KeyAction::Move(Direction::West)),
        "arrows can carry Fn"
    );
}

// ── Live focus (harness) ───────────────────────────────────────────────────

const KEY_RETURN: u8 = 36;
const KEY_DOWN: u8 = 125;
const KEY_RIGHT: u8 = 124;

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

fn selected(world: &World) -> Option<Entity> {
    world
        .get_resource::<Overview>()
        .expect("overview open")
        .selected
}

#[test]
fn test_overview_swap_while_open_keeps_the_window_selected() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            let (w0, w1) = (find_window_entity(0, world), find_window_entity(1, world));
            assert_eq!(
                row_windows(world, 0)[..2],
                [w1, w0],
                "swapped behind the overview"
            );
            assert_focused!(world, 0);
            assert_eq!(selected(world), Some(w0));
            let overview = world.get_resource::<Overview>().expect("still open");
            let x = |entity| overview.layout.find(entity).expect("tile").1.target.min.x;
            assert!(x(w1) < x(w0), "tiles re-projected in the new order");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            Event::Command {
                command: Command::Window(Operation::Swap(Direction::East)),
            },
        ]);
}

#[test]
fn test_overview_arrow_moves_real_focus_while_open() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            assert!(world.get_resource::<Overview>().is_some(), "still open");
            assert_focused!(world, 1);
            let w1 = find_window_entity(1, world);
            assert_eq!(selected(world), Some(w1));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            key(KEY_RIGHT),
        ]);
}

/// Windows 0 and 1 go to VW1, leaving window 2 alone and focused on VW0. Down
/// from a lone column switches the workspace behind the open overview.
#[test]
fn test_overview_arrow_down_switches_workspace_while_open() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(4, |world, _state| {
            assert!(world.get_resource::<Overview>().is_some(), "still open");
            assert_eq!(active_virtual_index(world), 1);
            let row = row_windows(world, 1);
            let selected = selected(world).expect("a selection");
            assert!(row.contains(&selected), "selection on the VW1 row");
            assert_eq!(Some(selected), focused_entity(world));
        })
        .on_iteration(5, |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_eq!(active_virtual_index(world), 1, "stays on VW1");
            let row = row_windows(world, 1);
            assert!(focused_entity(world).is_some_and(|focused| row.contains(&focused)));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            send_window_down(),
            send_window_down(),
            toggle(),
            key(KEY_DOWN),
            key(KEY_RETURN),
        ]);
}

#[test]
fn test_overview_projects_a_window_spawned_while_open() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, state| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
            state.focus_window(2);
        })
        .on_iteration(2, |world, _state| {
            let spawned = find_window_entity(2, world);
            let overview = world.get_resource::<Overview>().expect("still open");
            assert!(overview.layout.find(spawned).is_some(), "projected");
            assert_eq!(overview.selected, Some(spawned), "selected once focused");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            Event::MenuOpened { window_id: 0 },
        ]);
}

#[test]
fn test_overview_closes_when_the_active_display_changes() {
    TestHarness::new()
        .with_windows(1)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .on_iteration(1, |world, _state| {
            assert!(world.get_resource::<Overview>().is_some(), "open");
            // The harness has no real display switch, so move the marker.
            let mut query =
                world.query::<(Entity, &Display, bevy::ecs::query::Has<ActiveDisplayMarker>)>();
            let displays = query
                .iter(world)
                .map(|(entity, display, active)| (entity, display.id(), active))
                .collect::<Vec<_>>();
            for (entity, id, active) in displays {
                if active {
                    world.entity_mut(entity).remove::<ActiveDisplayMarker>();
                } else if id == EXT_DISPLAY_ID {
                    world.entity_mut(entity).insert(ActiveDisplayMarker);
                }
            }
        })
        .on_iteration(2, |world, _state| {
            assert!(
                world.get_resource::<Overview>().is_none(),
                "another display became active"
            );
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            Event::MenuOpened { window_id: 0 },
        ]);
}

// ── Closing (harness) ──────────────────────────────────────────────────────

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
        .on_iteration(2, move |world, _state| {
            assert!(world.get_resource::<Overview>().is_none(), "closed");
            assert_focused!(world, 0);
            assert_eq!(strip_snapshot(world), *before.borrow());
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            toggle(),
            key(KEY_ESCAPE),
        ]);
}

#[test]
fn test_overview_closing_the_focused_window_selects_the_new_focus() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, state| {
            let focused = find_window_entity(0, world);
            assert_eq!(selected(world), Some(focused));
            state.os_close_window(0);
        })
        .on_iteration(2, |world, _state| {
            let focused = focused_entity(world);
            let overview = world.get_resource::<Overview>().expect("still open");
            let tiles = overview
                .layout
                .rows
                .iter()
                .map(|row| row.tiles.len())
                .sum::<usize>();
            assert_eq!(tiles, 2, "the closed window's tile is gone");
            assert_eq!(overview.selected, focused, "the selection is the focus");
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
