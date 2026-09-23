//! Tests for the overview (`src/ecs/overview.rs`).

use bevy::ecs::entity::Entity;
use bevy::ecs::world::World;
use bevy::math::{IRect, IVec2};

use crate::commands::Command;
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::overview::{
    Overview, OverviewConfig, OverviewLayout, OverviewPhase, OverviewTile, project,
};
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
    let e = entities(4);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &e {
        strip.append(entity);
    }
    strip
        .stack(e[2])
        .expect("stack onto the column to the left");

    let layout = project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &unit_frame);
    let tiles = tiles_of(&layout, 0);

    assert_eq!(tiles.len(), 4);
    assert_eq!(
        tiles.iter().map(|(entity, _)| *entity).collect::<Vec<_>>(),
        e
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
    let e = entities(3);
    let strips = [2, 0, 1].map(|index| {
        let mut strip = LayoutStrip::new(1, index);
        strip.append(e[index as usize]);
        strip
    });
    let rows = [
        (e[0], &strips[0], false),
        (e[1], &strips[1], true),
        (e[2], &strips[2], false),
    ];
    let config = OverviewConfig {
        row_gap: 24,
        label_height: 20,
    };

    let layout = project(&rows, VIEWPORT, &config, &unit_frame);

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
    let e = entities(3);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &e {
        strip.append(entity);
    }
    let parked_corner = VIEWPORT.max - PARKED_STRIP_SLIVER;
    let parked = |entity: Entity| {
        let offset = e.iter().position(|&other| other == entity)? as i32 * 400;
        let min = parked_corner + IVec2::new(offset, 0);
        Some(IRect::from_corners(min, min + IVec2::new(400, 600)))
    };

    let visible = project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &unit_frame);
    let parked_layout = project(&[(e[0], &strip, false)], VIEWPORT, &NO_GAPS, &parked);

    assert_eq!(tiles_of(&visible, 0), tiles_of(&parked_layout, 0));
    assert_eq!(
        parked_layout.rows[0].tiles[0].origin.min, parked_corner,
        "origin is the real, parked frame"
    );
}

#[test]
fn test_overview_scrolled_column_projects_at_strip_position() {
    let e = entities(3);
    let mut strip = LayoutStrip::new(1, 0);
    for &entity in &e {
        strip.append(entity);
    }
    // The third column is scrolled off the right edge, down to a 5px sliver.
    let slivered = |entity: Entity| {
        if entity == e[2] {
            let x = VIEWPORT.max.x - 5;
            Some(IRect::new(x, 0, x + 400, 600))
        } else {
            unit_frame(entity)
        }
    };

    let layout = project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &slivered);
    let tiles = tiles_of(&layout, 0);

    assert_eq!(
        tiles,
        tiles_of(
            &project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &unit_frame),
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
    let e = entities(1);
    let strip = LayoutStrip::new(1, 0);

    let layout = project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &unit_frame);

    assert_eq!(layout.rows.len(), 1);
    assert!(layout.rows[0].tiles.is_empty());
    assert!(layout.rows[0].band.width() > 0 && layout.rows[0].band.height() > 0);
    assert!(
        project(&[], VIEWPORT, &NO_GAPS, &unit_frame)
            .rows
            .is_empty()
    );
}

#[test]
fn test_overview_fullscreen_and_tabs_are_one_tile() {
    let e = entities(4);
    let fullscreen = LayoutStrip::fullscreen(1, e[0]);
    let mut tabbed = LayoutStrip::new(1, 1);
    tabbed.append_tab_group(&e[1..4]);

    let layout = project(
        &[(e[0], &fullscreen, true), (e[1], &tabbed, false)],
        VIEWPORT,
        &NO_GAPS,
        &unit_frame,
    );

    assert_eq!(layout.rows[0].tiles.len(), 1);
    assert_eq!(layout.rows[0].tiles[0].tab_count, 1);
    assert_eq!(layout.rows[1].tiles.len(), 1);
    assert_eq!(layout.rows[1].tiles[0].entity, e[1]);
    assert_eq!(layout.rows[1].tiles[0].tab_count, 3);
}

#[test]
fn test_overview_zero_width_frames_do_not_panic() {
    let e = entities(2);
    let mut strip = LayoutStrip::new(1, 0);
    strip.append(e[0]);
    strip.append(e[1]);

    let layout = project(&[(e[0], &strip, true)], VIEWPORT, &NO_GAPS, &|_| {
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
