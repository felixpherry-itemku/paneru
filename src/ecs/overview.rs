//! The overview: a modal, zoomed-out map of every virtual workspace row on the
//! active display. Selection is deferred — nothing moves until the user commits.
//!
//! The `Overview` resource exists exactly while the overview is on screen, and
//! every system here is gated on it so nothing is scheduled while it is shut.

use bevy::app::{App, Plugin, PostUpdate, PreUpdate};
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Changed, Has};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::resource_exists;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use tracing::{Level, instrument};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::systems::ease_out_factor;
use crate::events::Event;
use crate::manager::Window;
use crate::overlay::{OverviewRenderer, OverviewScene, SceneRow, SceneTile};
use crate::platform::input::set_overview_active;

/// Escape. Always closes, so a swallowed keyboard can never get stuck.
const KEY_ESCAPE: u8 = 53;

/// How close `progress` must get to its goal before it snaps there.
const PROGRESS_EPSILON: f32 = 0.001;

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
    pub layout: OverviewLayout,
}

impl Overview {
    fn opening() -> Self {
        Self {
            phase: OverviewPhase::Opening,
            progress: 0.0,
            selected: None,
            layout: OverviewLayout::default(),
        }
    }
}

/// Pure projection output. No ECS state, no macOS types.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverviewLayout {
    pub rows: Vec<OverviewRow>,
}

/// One virtual workspace row.
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewRow {
    pub strip: Entity,
    pub virtual_index: u32,
    pub is_active: bool,
    /// Row band in absolute CG display coordinates.
    pub band: IRect,
    pub tiles: Vec<OverviewTile>,
}

/// One window, or one native tab group.
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewTile {
    pub entity: Entity,
    /// Where the tile settles, in absolute CG display coordinates.
    pub target: IRect,
    /// The window's real current frame, the animation's start point.
    pub origin: IRect,
    /// Greater than 1 when the tile stands for a native tab group.
    pub tab_count: usize,
}

impl OverviewTile {
    /// The tile's frame at `progress`, interpolating linearly from `origin`
    /// (0.0) to `target` (1.0).
    pub fn frame_at(&self, progress: f32) -> IRect {
        let t = f64::from(progress.clamp(0.0, 1.0));
        #[allow(clippy::cast_possible_truncation)]
        let lerp = |from: i32, to: i32| {
            (f64::from(from) + (f64::from(to) - f64::from(from)) * t).round() as i32
        };
        IRect::new(
            lerp(self.origin.min.x, self.target.min.x),
            lerp(self.origin.min.y, self.target.min.y),
            lerp(self.origin.max.x, self.target.max.x),
            lerp(self.origin.max.y, self.target.max.y),
        )
    }
}

/// The slice of [`Config`] the projection needs, so [`project`] stays free of
/// any config-reload coupling.
#[derive(Clone, Copy, Debug)]
pub struct OverviewConfig {
    /// Vertical gap between row bands, also used as the side margin.
    pub row_gap: i32,
    /// Space reserved at the top of each band for its label.
    pub label_height: i32,
}

impl From<&Config> for OverviewConfig {
    fn from(config: &Config) -> Self {
        Self {
            row_gap: config.overview_row_gap(),
            label_height: config.overview_label_height(),
        }
    }
}

/// Lays out every row of one Space as bands stacked down `viewport`, each
/// window a tile at its true strip-relative geometry scaled to fit its band.
///
/// `rows` is `(strip entity, strip, is_active)`. `frame_of` gives a window's
/// real frame: only its size feeds the layout (via
/// [`LayoutStrip::relative_positions`]), and the frame itself becomes the
/// tile's animation `origin`. Parked rows and slivered columns are moved, never
/// resized, so they project exactly like visible ones.
pub fn project<F>(
    rows: &[(Entity, &LayoutStrip, bool)],
    viewport: IRect,
    config: OverviewConfig,
    frame_of: &F,
) -> OverviewLayout
where
    F: Fn(Entity) -> Option<IRect>,
{
    let mut rows = rows.to_vec();
    rows.sort_by_key(|(_, strip, _)| strip.virtual_index);
    let Ok(count) = i32::try_from(rows.len()) else {
        return OverviewLayout::default();
    };

    let slice_height = viewport.height() / count.max(1);
    let rows = rows
        .iter()
        .zip(0..)
        .map(|(&(strip_entity, strip, is_active), index)| {
            let top = viewport.min.y + slice_height * index;
            let bottom = if index + 1 == count {
                viewport.max.y
            } else {
                top + slice_height
            };
            let half_gap = config.row_gap / 2;
            let band = IRect::new(
                viewport.min.x + config.row_gap,
                top + half_gap,
                viewport.max.x - config.row_gap,
                bottom - half_gap,
            );
            let mut inner = band;
            inner.min.y = (band.min.y + config.label_height).min(band.max.y);

            let rects = strip
                .relative_positions(viewport.height(), frame_of)
                .collect::<Vec<_>>();
            let tiles = fit_tiles(&rects, inner, frame_of);

            OverviewRow {
                strip: strip_entity,
                virtual_index: strip.virtual_index,
                is_active,
                band,
                tiles,
            }
        })
        .collect();
    OverviewLayout { rows }
}

/// Scales strip-local `rects` to fit inside `inner`, centred, never magnified.
fn fit_tiles<F>(rects: &[(Entity, IRect)], inner: IRect, frame_of: &F) -> Vec<OverviewTile>
where
    F: Fn(Entity) -> Option<IRect>,
{
    let bbox = rects
        .iter()
        .fold(IRect::new(0, 0, 0, 0), |bbox, (_, rect)| bbox.union(*rect));
    let bbox = IRect::from_corners(bbox.min.max(IVec2::ZERO), bbox.max);

    // An empty strip, or one whose windows report no width or height, has a
    // degenerate bbox: nothing to fit, so skip the division entirely.
    let scale = if bbox.width() > 0 && bbox.height() > 0 {
        (f64::from(inner.width()) / f64::from(bbox.width()))
            .min(f64::from(inner.height()) / f64::from(bbox.height()))
            .clamp(0.0, 1.0)
    } else {
        1.0
    };
    #[allow(clippy::cast_possible_truncation)]
    let scaled = |value: i32| (f64::from(value) * scale).round() as i32;
    let offset = inner.center() - IVec2::new(scaled(bbox.width()), scaled(bbox.height())) / 2;
    let place = |point: IVec2| {
        let local = point - bbox.min;
        offset + IVec2::new(scaled(local.x), scaled(local.y))
    };

    // `relative_positions` emits every member of a tab group back to back with
    // the same rect. Collapse each run into one tile standing for the group.
    let mut tiles: Vec<(IRect, OverviewTile)> = Vec::with_capacity(rects.len());
    for &(entity, rect) in rects {
        if let Some((last_rect, tile)) = tiles.last_mut()
            && *last_rect == rect
        {
            tile.tab_count += 1;
            continue;
        }
        let target = IRect::from_corners(place(rect.min), place(rect.max));
        let tile = OverviewTile {
            entity,
            target,
            origin: frame_of(entity).unwrap_or(target),
            tab_count: 1,
        };
        tiles.push((rect, tile));
    }
    tiles.into_iter().map(|(_, tile)| tile).collect()
}

/// One ease-out step of `progress` towards `goal`, snapping onto it once
/// within [`PROGRESS_EPSILON`] so the animation ends on exactly 0.0 or 1.0.
pub fn step_progress(progress: f32, goal: f32, t: f32) -> f32 {
    let next = progress + (goal - progress) * t;
    if (goal - next).abs() < PROGRESS_EPSILON {
        goal
    } else {
        next
    }
}

pub struct OverviewPlugin;

impl Plugin for OverviewPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, overview_toggle);
        app.add_systems(
            PostUpdate,
            // Per system, not once for the set: `overview_animate` can remove
            // the resource, and `overview_render` must then not run.
            (overview_project, overview_animate, overview_render)
                .chain()
                .distributive_run_if(resource_exists::<Overview>),
        );
    }
}

/// Opens the overview on `Command::Overview`; a second one starts it closing.
///
/// While open the event tap swallows the `overview` chord too, so it arrives as
/// an [`Event::OverviewKey`]; that — or Escape — closes it as well.
#[instrument(level = Level::DEBUG, skip_all)]
fn overview_toggle(
    mut messages: MessageReader<Event>,
    overview: Option<ResMut<Overview>>,
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

    if let Some(mut overview) = overview {
        // Already on its way out: nothing more to do.
        if !matches!(overview.phase, OverviewPhase::Closing { .. }) {
            overview.phase = OverviewPhase::Closing { activate: None };
        }
    } else {
        commands.insert_resource(Overview::opening());
        set_overview_active(true);
    }
}

/// Projects the active display's rows into the overview on open, and again
/// whenever a strip changes or a window goes away while it is up.
#[instrument(level = Level::DEBUG, skip_all)]
fn overview_project(
    mut overview: ResMut<Overview>,
    active_display: ActiveDisplay,
    strips: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    changed_strips: Query<(), Changed<LayoutStrip>>,
    mut removed_windows: RemovedComponents<Window>,
    windows: Windows,
    config: Res<Config>,
) {
    let window_removed = removed_windows.read().count() > 0;
    if !overview.is_added() && changed_strips.is_empty() && !window_removed {
        return;
    }

    let workspace_id = active_display.active_strip().id();
    let rows = strips
        .iter()
        .filter(|(_, strip, _)| strip.id() == workspace_id)
        .collect::<Vec<_>>();
    overview.layout = project(
        &rows,
        active_display.actual_bounds(&config),
        OverviewConfig::from(config.as_ref()),
        &|entity| windows.moving_frame(entity),
    );
}

/// Hands the renderer a fresh scene whenever the overview changed: every frame
/// of the animation, and on projection or selection changes once settled.
#[instrument(level = Level::DEBUG, skip_all)]
fn overview_render(
    overview: Res<Overview>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    renderer: Option<NonSendMut<OverviewRenderer>>,
) {
    let Some(mut renderer) = renderer else {
        return;
    };
    if !overview.is_changed() {
        return;
    }

    let display = active_display.display();
    let mut bounds = display.bounds();
    bounds.min.y -= display.menubar_height();

    let rows = overview
        .layout
        .rows
        .iter()
        .map(|row| SceneRow {
            band: row.band,
            label: (row.virtual_index + 1).to_string(),
            is_active: row.is_active,
        })
        .collect();
    let tiles = overview
        .layout
        .rows
        .iter()
        .flat_map(|row| &row.tiles)
        .map(|tile| {
            let window = windows.get(tile.entity);
            SceneTile {
                pid: window
                    .and_then(|window| window.pid().ok())
                    .unwrap_or_default(),
                frame: tile.frame_at(overview.progress),
                title: window
                    .and_then(|window| window.title().ok())
                    .unwrap_or_default(),
                tab_count: tile.tab_count,
                selected: overview.selected == Some(tile.entity),
            }
        })
        .collect();

    renderer.render(OverviewScene {
        display: bounds,
        progress: overview.progress,
        scrim_opacity: config.overview_scrim_opacity(),
        scrim_color: config.overview_scrim_color(),
        label_height: config.overview_label_height(),
        rows,
        tiles,
    });
}

/// Drives `progress` towards 1.0 while opening and 0.0 while closing. A
/// settled, open overview is left untouched so nothing downstream sees a change.
#[instrument(level = Level::DEBUG, skip_all)]
fn overview_animate(
    mut overview: ResMut<Overview>,
    time: Res<Time>,
    config: Res<Config>,
    renderer: Option<NonSendMut<OverviewRenderer>>,
    mut commands: Commands,
) {
    let goal = match overview.phase {
        OverviewPhase::Open => return,
        OverviewPhase::Opening => 1.0,
        OverviewPhase::Closing { .. } => 0.0,
    };
    let t = ease_out_factor(config.overview_animation_speed(), time.delta_secs_f64());
    overview.progress = step_progress(overview.progress, goal, t);

    match overview.phase {
        OverviewPhase::Opening if overview.progress >= 1.0 => {
            overview.phase = OverviewPhase::Open;
        }
        OverviewPhase::Closing { .. } if overview.progress <= 0.0 => {
            finish_close(&mut commands, renderer);
        }
        _ => {}
    }
}

/// The single exit from the overview: gives the keyboard back, takes the
/// window down and drops the resource, which also unschedules every overview
/// system.
fn finish_close(commands: &mut Commands, renderer: Option<NonSendMut<OverviewRenderer>>) {
    set_overview_active(false);
    if let Some(mut renderer) = renderer {
        renderer.close();
    }
    commands.remove_resource::<Overview>();
}
