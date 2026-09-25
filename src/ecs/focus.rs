use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Populated, Query, Res, ResMut, Single};
use bevy::math::IRect;
use bevy::prelude::Event as BevyEvent;
use bevy::time::common_conditions::on_timer;
use tracing::{Level, debug, error, instrument, trace, warn};

use super::{FocusedMarker, MouseHeldMarker, SystemTheme, Unmanaged};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, GlobalState, WindowCtx, Windows};
use crate::ecs::workspace::{RestoreFocusMarker, column_closest_to_center};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, Position, RaiseWindow, ResizeMarker, Scrolling,
    SendMessageTrigger, SpawnCommandsExt, StrayFocusEvent,
};
use crate::events::Event;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::WorkspaceId;

const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;

/// How long after losing focus a closing window still counts as focused.
/// An app that closes one of several windows can focus its own next window
/// before the close reaches us. // ponytail: fixed heuristic — a window that
/// closes by itself within this long after the user moved off it still hands
/// focus on; tie it to the same app if that ever bites.
const CLOSE_FOCUS_GRACE: Duration = Duration::from_millis(500);

/// The tiled window that was `last_managed` before the current one.
#[derive(Clone, Copy)]
struct Handover {
    entity: Entity,
    return_to: Option<Entity>,
    at: Duration,
}

#[derive(Default)]
pub struct TierMemory {
    pub last_managed: Option<Entity>,
    pub last_floating: Option<Entity>,
    /// niri's `activate_prev_column_on_removal`: the column `last_managed` was
    /// opened or tiled back beside. If `last_managed` closes or floats again
    /// before another tiled window is focused, this column becomes the active
    /// one again.
    return_to: Option<Entity>,
    previous: Option<Handover>,
}

/// Keyed by `WorkspaceId` so toggling on one Space can't reach a window last
/// focused on another. Cleared on entity despawn (`forget`) so recycled
/// Entity IDs can't resolve to the wrong window, and on workspace despawn
/// (`forget_workspace`) to bound the map.
#[derive(Default, Resource)]
pub struct FocusHistory {
    pub pending_focus: Option<Entity>,
    by_workspace: HashMap<WorkspaceId, TierMemory>,
}

impl FocusHistory {
    pub fn record(
        &mut self,
        workspace: WorkspaceId,
        entity: Entity,
        unmanaged: Option<&Unmanaged>,
        now: Duration,
    ) {
        let slot = self.by_workspace.entry(workspace).or_default();
        match unmanaged {
            None => {
                // Re-recording the same window (the OS echoing focus) keeps
                // the flag; any other tiled window taking focus drops it, but
                // the outgoing one keeps its flag in `previous` in case it is
                // closing.
                if slot.last_managed != Some(entity) {
                    slot.previous = slot.last_managed.map(|entity| Handover {
                        entity,
                        return_to: slot.return_to,
                        at: now,
                    });
                    slot.return_to = None;
                }
                slot.last_managed = Some(entity);
            }
            Some(Unmanaged::Floating) => slot.last_floating = Some(entity),
            Some(_) => {}
        }
    }

    /// `entity` was tiled back as a new column right of `anchor` and has focus:
    /// it becomes the active column, and floating it again returns to `anchor`.
    pub fn tiled_beside(&mut self, workspace: WorkspaceId, entity: Entity, anchor: Option<Entity>) {
        let slot = self.by_workspace.entry(workspace).or_default();
        slot.last_managed = Some(entity);
        slot.return_to = anchor;
    }

    /// `entity` left the strip of `workspace` by closing (`now` is `Some`) or
    /// floating (`None`). If it was the active column, or is closing and lost
    /// that role within `CLOSE_FOCUS_GRACE`, the role goes to the column it was
    /// opened or tiled back beside (when that is `still_there`), otherwise to
    /// `successor`. Returns the new active column; `None` means `entity` wasn't
    /// the active column and focus must not move.
    pub fn hand_off(
        &mut self,
        workspace: WorkspaceId,
        entity: Entity,
        now: Option<Duration>,
        still_there: impl Fn(Entity) -> bool,
        successor: Option<Entity>,
    ) -> Option<Entity> {
        let slot = self.by_workspace.get_mut(&workspace)?;
        let anchor = if slot.last_managed == Some(entity) {
            slot.return_to
        } else if let Some(now) = now
            && let Some(previous) = slot.previous.filter(|previous| {
                previous.entity == entity && now.saturating_sub(previous.at) <= CLOSE_FOCUS_GRACE
            })
        {
            previous.return_to
        } else {
            if slot.return_to == Some(entity) {
                slot.return_to = None;
            }
            return None;
        };
        slot.last_managed = anchor.filter(|column| still_there(*column)).or(successor);
        slot.return_to = None;
        slot.previous = None;
        slot.last_managed
    }

    pub fn last_managed(&self, workspace: WorkspaceId) -> Option<Entity> {
        self.by_workspace
            .get(&workspace)
            .and_then(|t| t.last_managed)
    }

    pub fn last_floating(&self, workspace: WorkspaceId) -> Option<Entity> {
        self.by_workspace
            .get(&workspace)
            .and_then(|t| t.last_floating)
    }

    pub fn forget(&mut self, entity: Entity) {
        if self.pending_focus == Some(entity) {
            self.pending_focus = None;
        }
        for slot in self.by_workspace.values_mut() {
            if slot.last_managed == Some(entity) {
                slot.last_managed = None;
                slot.return_to = None;
            }
            if slot.return_to == Some(entity) {
                slot.return_to = None;
            }
            if slot.last_floating == Some(entity) {
                slot.last_floating = None;
            }
            slot.previous.take_if(|previous| previous.entity == entity);
            if let Some(previous) = &mut slot.previous
                && previous.return_to == Some(entity)
            {
                previous.return_to = None;
            }
        }
    }

    pub fn forget_workspace(&mut self, workspace: WorkspaceId) {
        self.by_workspace.remove(&workspace);
    }
}

pub struct FocusEventsPlugin;

impl Plugin for FocusEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusHistory>();
        app.add_systems(Update, (detect_focus_rejection, fix_window_size_on_focus));
        app.add_systems(
            PostUpdate,
            (
                autocenter_window_on_focus.after(super::systems::animate_resize_entities),
                mouse_follows_focus.after(super::systems::animate_resize_entities),
                recover_lost_focus.run_if(on_timer(Duration::from_millis(
                    REFRESH_WINDOW_CHECK_FREQ_MS,
                ))),
            ),
        );
        app.add_observer(dim_remove_window_trigger)
            .add_observer(dim_window_trigger)
            .add_observer(maintain_focus_singleton)
            .add_observer(virtual_strip_activated)
            .add_observer(stray_focus_observer)
            .add_observer(focus_window_trigger)
            .add_observer(raise_window_trigger);
    }
}

#[derive(BevyEvent)]
pub(super) struct FocusWindow {
    pub entity: Entity,
    pub raise: bool,
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn maintain_focus_singleton(
    trigger: On<Add, FocusedMarker>,
    windows: Query<(Entity, Has<FocusedMarker>), With<Window>>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    let focused_entity = trigger.event().entity;

    for (entity, focused) in windows {
        if focused
            && entity != focused_entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!("window {entity} lost focus.");
            entity_commands.try_remove::<FocusedMarker>();
        }
    }

    // Check if the reshuffle was caused by a keyboard switch or mouse move.
    // Skip reshuffle if caused by mouse - because then it won't center.
    if config.ffm_flag().is_none() {
        config.set_skip_reshuffle(false);
    }
    config.set_ffm_flag(None);
}

/// Whether two windows are members of one native tab group: the app shows one
/// of them at a time, and focusing any of them can leave the focus on the one
/// the app decided to show.
///
/// The strip knows the ones it has already grouped. The rest are recognised the
/// same way [`super::systems::detect_tabbed_windows`] recognises them in the
/// first place: same app, same frame.
fn shares_a_tab_group(
    workspaces: &Query<(Entity, &mut LayoutStrip)>,
    windows: &Windows,
    target: Entity,
    actual: Entity,
) -> bool {
    if workspaces.iter().any(|(_, strip)| {
        strip
            .tab_group(target)
            .is_some_and(|group| group.contains(&actual))
    }) {
        return true;
    }

    let parent_of = |entity: Entity| {
        windows
            .get(entity)
            .and_then(|window| windows.find_parent(window.id()))
            .map(|(_, _, parent)| parent)
    };
    let (Some(target_app), Some(actual_app)) = (parent_of(target), parent_of(actual)) else {
        return false;
    };
    if target_app != actual_app {
        return false;
    }

    windows
        .frame(target)
        .zip(windows.frame(actual))
        .is_some_and(|(target_frame, actual_frame)| {
            target_frame.min.chebyshev_distance(actual_frame.min) <= 1
                && target_frame.size().chebyshev_distance(actual_frame.size()) <= 1
        })
}

#[instrument(level = Level::DEBUG, skip_all, fields(focused))]
fn fix_window_size_on_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    mut windows: Query<(&mut Window, &mut Bounds, Has<ResizeMarker>)>,
) {
    if let Ok((mut window, mut bounds, resizing)) = windows.get_mut(*focused)
        && !resizing
        && let Ok(frame) = window.update_frame()
        && frame.size() != bounds.0
    {
        debug!("fixing window {} size!", window.id());
        bounds.0 = frame.size();
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(focused))]
fn detect_focus_rejection(
    focused: Single<Entity, Added<FocusedMarker>>,
    mut focus_history: ResMut<FocusHistory>,
    mut workspaces: Query<(Entity, &mut LayoutStrip)>,
    windows: Windows,
    mut commands: Commands,
) {
    let Some(target_entity) = focus_history.pending_focus.take() else {
        return;
    };
    if *focused == target_entity {
        return;
    }

    // Native tabs share one slot: asking for a background tab makes the app
    // select it, and the focus notification names whichever tab of the group
    // the app ended up showing. That is the app doing what was asked, not
    // refusing it — floating the window here is how a tabbed terminal ends up
    // scattered across the layout as windows nothing tiles.
    if shares_a_tab_group(&workspaces, &windows, target_entity, *focused) {
        debug!(
            "focus landed on tab sibling {} of {target_entity}; not a rejection.",
            *focused
        );
        return;
    }

    debug!(
        "focus rejection detected: requested {target_entity}, got {}. Floating {target_entity}.",
        *focused
    );
    if let Ok(mut entity_commands) = commands.get_entity(target_entity) {
        entity_commands.try_insert(Unmanaged::Floating);
    }
    for (_, mut strip) in &mut workspaces {
        if strip.contains(target_entity) {
            strip.remove(target_entity);
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn autocenter_window_on_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    mouse_held: Query<&MouseHeldMarker>,
    restored: Query<&RestoreFocusMarker>,
    global_state: GlobalState,
    active_display: ActiveDisplay,
    mut ctx: WindowCtx,
) {
    let entity = *focused;

    // Skip auto-centering when this focus came from a workspace restore, since
    // the strip is already at its saved origin. window_focused_trigger and
    // timeout_ticker are responsible for clearing the marker.
    if restored.iter().any(|marker| marker.entity == entity) {
        return;
    }

    if global_state.skip_reshuffle() || global_state.initializing() || !mouse_held.is_empty() {
        return;
    }
    if active_display.active_strip().tabbed(entity) {
        return;
    }
    if ctx.config.auto_center()
        && let Some((_, _, None)) = ctx.windows.get_managed(entity)
        && let Some(size) = ctx.windows.size(entity)
        && let Some(mut origin) = ctx.windows.origin(entity)
    {
        let center = active_display.bounds().center();
        origin.x = center.x - size.x / 2;
        ctx.commands.reposition_entity(entity, origin);
    }
    ctx.commands.reshuffle_around(entity);
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn mouse_follows_focus(
    focused: Single<Entity, Added<FocusedMarker>>,
    windows: Windows,
    global_state: GlobalState,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    displays: Query<&Display>,
    workspaces: Query<(
        &LayoutStrip,
        &ChildOf,
        Option<&Scrolling>,
        Has<ActiveWorkspaceMarker>,
    )>,
) {
    let entity = *focused;
    let Some(window) = windows.get(entity) else {
        return;
    };
    if workspaces
        .iter()
        .find_map(|(_, _, scrolling, active)| if active { scrolling } else { None })
        .is_some_and(|scrolling| scrolling.is_user_swiping)
    {
        debug!("Suppressing center mouse due to a swipe");
        return;
    }

    trace!(
        "window {}, skip_reshuffle {}, ffm flag {:?}.",
        window.id(),
        global_state.skip_reshuffle(),
        global_state.ffm_flag()
    );
    if config.mouse_follows_focus()
        && !global_state.skip_reshuffle()
        && global_state.ffm_flag().is_none_or(|id| id != window.id())
        && let Some(frame) = windows.moving_frame(entity)
        && let Some(display_bounds) = workspaces
            .into_iter()
            .find_map(|(strip, child, _, _)| strip.contains(entity).then_some(child))
            .and_then(|child| displays.get(child.parent()).ok())
            .map(Display::bounds)
    {
        let visible = display_bounds.intersect(frame);
        // If the overlap is smaller than 50x50, the window is probably hidden
        // off screen, so do not move the mouse.
        if visible.size().length_squared() > 5000 {
            let origin = visible.center();
            debug!("centering on {} {origin}", window.id());
            window_manager.warp_mouse(origin);
        }
    }
}

fn dim_window_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if config.window_dim_ratio(dark).is_some() {
        window_manager.dim_windows(&[window.id()], 0.0);
    }
}

fn dim_remove_window_trigger(
    trigger: On<Remove, FocusedMarker>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some((window, _, None)) = windows.get_managed(trigger.event().entity) else {
        return;
    };

    let same_display = active_display
        .active_strip()
        .contains(trigger.event().entity);
    if !same_display {
        // Do not dim the window loosing focus on another display.
        return;
    }

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if let Some(dim_ratio) = config.window_dim_ratio(dark) {
        window_manager.dim_windows(&[window.id()], dim_ratio);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn virtual_strip_activated(
    trigger: On<Add, FocusedMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    let owner_strip = workspaces.into_iter().find_map(|(entity, strip, active)| {
        (strip.contains(trigger.entity) && !active).then_some(entity)
    });
    if let Some(entity) = owner_strip
        && let Ok(mut entity_commands) = commands.get_entity(entity)
    {
        entity_commands.try_insert(ActiveWorkspaceMarker);
    }
}

fn focus_window_trigger(trigger: On<FocusWindow>, windows: Windows, apps: Query<&Application>) {
    let FocusWindow { entity, raise } = *trigger.event();
    let Some(window) = windows.get(entity) else {
        return;
    };
    let Some(psn) = windows.psn(window.id(), &apps) else {
        return;
    };
    if !raise
        && let Some((focused_window, _)) = windows.focused()
        && let Some(focused_psn) = windows.psn(focused_window.id(), &apps)
    {
        window.focus_without_raise(psn, focused_window, focused_psn);
    } else {
        window.focus_with_raise(psn);
    }
}

fn raise_window_trigger(
    trigger: On<RaiseWindow>,
    windows: Query<(Entity, &Window, &Position, &Bounds)>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let RaiseWindow { entity, with_strip } = *trigger.event();

    let Ok((focus, window, _, _)) = windows.get(entity) else {
        return;
    };

    if with_strip {
        let viewport = active_display.actual_bounds(&config);
        let strip = active_display.active_strip();
        strip
            .all_windows()
            .into_iter()
            .filter_map(|entity| {
                if entity == focus {
                    None
                } else {
                    windows.get(entity).ok()
                }
            })
            .filter(|(_, _, origin, size)| {
                let frame = IRect::from_corners(origin.0, origin.0 + size.0);
                viewport.intersect(frame).width() > 50
            })
            .for_each(|(_, window, _, _)| {
                window.raise_without_focus();
            });
    }

    // Raise the focused window last, because raised windows get OS focus events.
    window.raise_without_focus();
}

#[instrument(level = Level::DEBUG, skip_all)]
fn recover_lost_focus(
    windows: Windows,
    active_display: ActiveDisplay,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    let strip = active_display.active_strip();
    // An empty row has nothing to recover to.
    if windows.focused().is_some() || strip.len() == 0 {
        return;
    }
    error!("Lost focus marker, recovering!");
    let target = focus_history
        .last_managed(strip.id())
        .filter(|column| strip.contains(*column))
        .or_else(|| column_closest_to_center(strip, active_display.display(), &windows));
    if let Some(entity) = target {
        commands.focus_entity(entity, false);
    }
}

pub(super) fn stray_focus_observer(
    trigger: On<Add, Window>,
    focus_events: Populated<(Entity, &StrayFocusEvent)>,
    windows: Windows,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    let Some(window_id) = windows.get(entity).map(|window| window.id()) else {
        return;
    };

    focus_events
        .iter()
        .filter(|(_, stray_focus)| stray_focus.0 == window_id)
        .for_each(|(timeout_entity, _)| {
            debug!("Re-queueing lost focus event for window id {window_id}.");
            commands.trigger(SendMessageTrigger(Event::WindowFocused { window_id }));
            if let Ok(mut entity_commands) = commands.get_entity(timeout_entity) {
                entity_commands.try_despawn();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::world::World;

    #[test]
    fn record_and_read_per_tier() {
        let mut world = World::new();
        let managed = world.spawn(()).id();
        let floating = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, managed, None, Duration::ZERO);
        history.record(1, floating, Some(&Unmanaged::Floating), Duration::ZERO);

        assert_eq!(history.last_managed(1), Some(managed));
        assert_eq!(history.last_floating(1), Some(floating));
    }

    #[test]
    fn record_ignores_minimized_and_hidden() {
        let mut world = World::new();
        let entity = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, entity, Some(&Unmanaged::Minimized), Duration::ZERO);
        history.record(1, entity, Some(&Unmanaged::Hidden), Duration::ZERO);

        assert_eq!(history.last_managed(1), None);
        assert_eq!(history.last_floating(1), None);
    }

    #[test]
    fn per_workspace_isolation() {
        let mut world = World::new();
        let a = world.spawn(()).id();
        let b = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, a, None, Duration::ZERO);
        history.record(2, b, None, Duration::ZERO);

        assert_eq!(history.last_managed(1), Some(a));
        assert_eq!(history.last_managed(2), Some(b));
    }

    #[test]
    fn forget_clears_entity_across_workspaces() {
        let mut world = World::new();
        let target = world.spawn(()).id();
        let other = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, target, None, Duration::ZERO);
        history.record(2, target, Some(&Unmanaged::Floating), Duration::ZERO);
        history.record(2, other, None, Duration::ZERO);

        history.forget(target);

        assert_eq!(history.last_managed(1), None);
        assert_eq!(history.last_floating(2), None);
        assert_eq!(history.last_managed(2), Some(other));
    }

    fn return_to(history: &FocusHistory, workspace: WorkspaceId) -> Option<Entity> {
        history
            .by_workspace
            .get(&workspace)
            .and_then(|slot| slot.return_to)
    }

    #[test]
    fn tiled_beside_activates_the_window_and_remembers_the_anchor() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.tiled_beside(1, window, Some(anchor));

        assert_eq!(history.last_managed(1), Some(window));
        assert_eq!(return_to(&history, 1), Some(anchor));
    }

    #[test]
    fn record_clears_return_to_only_when_another_tiled_window_takes_focus() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let other = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));

        history.record(1, window, None, Duration::ZERO);
        history.record(1, other, Some(&Unmanaged::Floating), Duration::ZERO);
        assert_eq!(return_to(&history, 1), Some(anchor));

        history.record(1, other, None, Duration::ZERO);
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn hand_off_returns_to_the_anchor_when_flagged() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));

        assert_eq!(
            history.hand_off(1, window, None, |_| true, Some(successor)),
            Some(anchor)
        );

        assert_eq!(history.last_managed(1), Some(anchor));
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn hand_off_goes_to_the_successor_without_a_flag() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.record(1, window, None, Duration::ZERO);

        history.hand_off(1, window, None, |_| true, Some(successor));

        assert_eq!(history.last_managed(1), Some(successor));
    }

    #[test]
    fn hand_off_skips_a_stale_anchor() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));

        history.hand_off(1, window, None, |column| column != anchor, Some(successor));

        assert_eq!(history.last_managed(1), Some(successor));
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn hand_off_of_the_anchor_clears_the_flag() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));

        assert_eq!(
            history.hand_off(1, anchor, None, |_| true, Some(successor)),
            None
        );

        assert_eq!(history.last_managed(1), Some(window));
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn hand_off_on_an_unknown_space_is_a_no_op() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.hand_off(1, window, None, |_| true, Some(window));

        assert!(history.by_workspace.is_empty());
    }

    fn previous(
        history: &FocusHistory,
        workspace: WorkspaceId,
    ) -> Option<(Entity, Option<Entity>)> {
        history
            .by_workspace
            .get(&workspace)
            .and_then(|slot| slot.previous)
            .map(|previous| (previous.entity, previous.return_to))
    }

    #[test]
    fn record_keeps_the_outgoing_window_and_its_anchor_in_previous() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let other = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));

        history.record(1, other, None, Duration::from_secs(1));

        assert_eq!(previous(&history, 1), Some((window, Some(anchor))));
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn closing_within_the_grace_returns_to_the_anchor_else_the_successor() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let other = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let closed_at = Some(Duration::from_millis(1500));

        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));
        history.record(1, other, None, Duration::from_secs(1));
        assert_eq!(
            history.hand_off(1, window, closed_at, |_| true, Some(successor)),
            Some(anchor)
        );
        assert_eq!(history.last_managed(1), Some(anchor));
        assert_eq!(previous(&history, 1), None);

        let mut history = FocusHistory::default();
        history.tiled_beside(1, window, Some(anchor));
        history.record(1, other, None, Duration::from_secs(1));
        assert_eq!(
            history.hand_off(
                1,
                window,
                closed_at,
                |column| column != anchor,
                Some(successor)
            ),
            Some(successor)
        );
    }

    #[test]
    fn no_grace_once_it_expired_or_when_floating() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let other = world.spawn(()).id();
        let successor = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.record(1, window, None, Duration::ZERO);
        history.record(1, other, None, Duration::from_secs(1));

        let expired = Some(Duration::from_millis(1501));
        assert_eq!(
            history.hand_off(1, window, expired, |_| true, Some(successor)),
            None
        );
        assert_eq!(
            history.hand_off(1, window, None, |_| true, Some(successor)),
            None
        );
        assert_eq!(history.last_managed(1), Some(other));
    }

    #[test]
    fn hand_off_of_a_window_that_was_never_active_leaves_the_column() {
        let mut world = World::new();
        let active = world.spawn(()).id();
        let background = world.spawn(()).id();
        let mut history = FocusHistory::default();
        history.record(1, active, None, Duration::ZERO);

        assert_eq!(
            history.hand_off(1, background, Some(Duration::ZERO), |_| true, Some(active)),
            None
        );
        assert_eq!(history.last_managed(1), Some(active));
    }

    #[test]
    fn forget_clears_previous() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let other = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.tiled_beside(1, window, Some(anchor));
        history.record(1, other, None, Duration::ZERO);
        history.forget(anchor);
        assert_eq!(previous(&history, 1), Some((window, None)));

        history.forget(window);
        assert_eq!(previous(&history, 1), None);
    }

    #[test]
    fn forget_clears_return_to() {
        let mut world = World::new();
        let window = world.spawn(()).id();
        let anchor = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.tiled_beside(1, window, Some(anchor));
        history.forget(anchor);
        assert_eq!(history.last_managed(1), Some(window));
        assert_eq!(return_to(&history, 1), None);

        history.tiled_beside(1, window, Some(anchor));
        history.forget(window);
        assert_eq!(history.last_managed(1), None);
        assert_eq!(return_to(&history, 1), None);
    }

    #[test]
    fn forget_workspace_drops_entry() {
        let mut world = World::new();
        let entity = world.spawn(()).id();
        let mut history = FocusHistory::default();

        history.record(1, entity, None, Duration::ZERO);
        history.forget_workspace(1);

        assert_eq!(history.last_managed(1), None);
    }

    /// Native tabs share a slot: the app answering with a sibling of the tab
    /// group is it doing what was asked, so the requested window must keep its
    /// place in the layout.
    #[test]
    fn focus_landing_on_a_tab_sibling_is_not_a_rejection() {
        let mut world = World::new();
        let target = world.spawn(()).id();
        let sibling = world.spawn(()).id();

        let mut strip = LayoutStrip::default();
        strip.append(target);
        strip
            .convert_to_tabs(target, sibling)
            .expect("target is in the strip");
        world.spawn(strip);

        world.insert_resource(FocusHistory {
            pending_focus: Some(target),
            ..Default::default()
        });
        let system_id = world.register_system(detect_focus_rejection);

        world.entity_mut(sibling).insert(FocusedMarker);
        _ = world.run_system(system_id);

        assert!(
            world.get::<Unmanaged>(target).is_none(),
            "a tab sibling taking the focus must not float the requested tab"
        );
        let mut strips = world.query::<&LayoutStrip>();
        assert!(
            strips.single(&world).expect("one strip").contains(target),
            "and must not take it out of the layout"
        );
        assert_eq!(world.resource::<FocusHistory>().pending_focus, None);
    }

    #[test]
    fn focus_rejection_floats_target_and_clears_pending_focus() {
        let mut world = World::new();
        let target = world.spawn(()).id();
        let actual = world.spawn(()).id();

        let mut strip = LayoutStrip::default();
        strip.append(target);
        strip.append(actual);
        world.spawn(strip);

        let history = FocusHistory {
            pending_focus: Some(target),
            ..Default::default()
        };
        world.insert_resource(history);

        let system_id = world.register_system(detect_focus_rejection);

        // Focus arrives on actual instead of requested target
        world.entity_mut(actual).insert(FocusedMarker);
        _ = world.run_system(system_id);

        assert!(
            world
                .get::<Unmanaged>(target)
                .is_some_and(|u| matches!(u, Unmanaged::Floating))
        );
        assert_eq!(world.resource::<FocusHistory>().pending_focus, None);
    }
}
