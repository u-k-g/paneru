use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Populated, Res, Single, SystemParam};
use bevy::math::{IRect, IVec2};
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{Level, instrument};

use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::config::swipe::SwipeGestureDirection;
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, MissionControlActive, Position, Scrolling, SendMessageTrigger,
    SpawnCommandsExt,
};
use crate::errors::Result as PaneruResult;
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;

pub struct ScrollEventsPlugin;

const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
// Keep snap intent longer than the general scroll timeout so a brief pause in
// gesture samples cannot strand the strip before macOS reports finger-up.
const SNAP_GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

impl Plugin for ScrollEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            Update,
            (
                vertical_swipe_gesture.run_if(mission_control_inactive),
                (
                    swipe_gesture.run_if(mission_control_inactive),
                    apply_inertia,
                    apply_snap_force,
                    scrolling_integrator,
                    apply_scrolling_constraints,
                    snap_trackpad_swipe,
                    swiping_timeout,
                )
                    .chain(),
            ),
        );
    }
}

#[derive(SystemParam)]
struct SwipeGestureParams<'w, 's> {
    active_display: ActiveDisplay<'w, 's>,
    active_workspace: Single<
        'w,
        's,
        (Entity, &'static Position, Option<&'static mut Scrolling>),
        With<ActiveWorkspaceMarker>,
    >,
    windows: Windows<'w, 's>,
    time: Res<'w, Time>,
    config: Res<'w, Config>,
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
#[instrument(level = Level::TRACE, skip_all)]
fn swipe_gesture(
    mut messages: MessageReader<Event>,
    params: SwipeGestureParams,
    mut commands: Commands,
) {
    let SwipeGestureParams {
        active_display,
        mut active_workspace,
        windows,
        time,
        config,
    } = params;
    let swipe_sensitivity = config.swipe_sensitivity();
    let mut total_delta = 0.0;
    let mut gesture_delta = 0.0;
    let mut gesture_fingers = None;
    let mut touchpad_down = false;
    let mut touchpad_up = false;
    let mut has_scroll_event = false;
    let mut has_gesture_event = false;

    let scroll_scale = modifier_scroll_scale(swipe_sensitivity);

    for event in messages.read() {
        match event {
            Event::TouchpadDown => {
                touchpad_down = true;
                total_delta = 0.0;
            }
            Event::TouchpadUp => touchpad_up = true,
            Event::Scroll { delta } => {
                total_delta += *delta * scroll_scale;
                has_scroll_event = true;
            }
            Event::Swipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                total_delta += delta;
                gesture_delta += delta;
                has_scroll_event = true;
                has_gesture_event = true;
                gesture_fingers = Some(*fingers);
            }
            _ => (),
        }
    }

    if !touchpad_down && !touchpad_up && !has_scroll_event {
        return;
    }

    let (entity, position, scrolling) = &mut *active_workspace;
    let focused_at_start = windows.focused().map(|(_, entity)| entity);

    if touchpad_down && let Some(scrolling) = scrolling.as_mut() {
        scrolling.velocity = 0.0;
        scrolling.is_user_swiping = true;
        scrolling.fingers_count = None;
        scrolling.started_focused = focused_at_start;
        scrolling.last_event = Instant::now();
    }

    if touchpad_up
        && let Some(scrolling) = scrolling.as_mut()
        && !is_snap_gesture(scrolling, &config)
    {
        scrolling.is_user_swiping = false;
        scrolling.fingers_count = None;
        scrolling.started_focused = None;
    }

    if has_scroll_event {
        let viewport_width = f64::from(active_display.bounds().width());
        let direction_modifier = match config.swipe_gesture_direction() {
            SwipeGestureDirection::Natural => -1.0,
            SwipeGestureDirection::Reversed => 1.0,
        };

        let dt = time.delta_secs_f64();
        let new_velocity = if has_gesture_event && dt > 0.0 {
            gesture_delta * swipe_sensitivity / dt
        } else {
            0.0
        };

        if let Some(scrolling) = scrolling.as_mut() {
            // Native modifier-scroll events already include macOS momentum.
            // Add synthetic inertia only for raw multi-finger gestures.
            scrolling.velocity = if has_gesture_event {
                // Smoothen gesture velocity changes using EMA.
                0.3 * new_velocity + 0.7 * scrolling.velocity
            } else {
                0.0
            };
            scrolling.is_user_swiping = true;
            if has_gesture_event {
                scrolling.fingers_count = gesture_fingers;
            }
            scrolling.last_event = Instant::now();
            scrolling.position +=
                total_delta * viewport_width * direction_modifier * swipe_sensitivity;
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            let resulting_position = f64::from(position.0.x)
                + total_delta * viewport_width * direction_modifier * swipe_sensitivity;
            entity_commands.try_insert(Scrolling {
                velocity: new_velocity,
                position: resulting_position,
                is_user_swiping: true,
                fingers_count: gesture_fingers,
                started_focused: focused_at_start,
                last_event: Instant::now(),
            });
        }
    }
}

fn modifier_scroll_scale(swipe_sensitivity: f64) -> f64 {
    // Native modifier-scroll deltas are larger than raw touch deltas.
    const UPPER: f64 = 0.15;
    const LOWER: f64 = 0.005;
    const FULL_RANGE: f64 = 2.0;
    LOWER + ((UPPER - LOWER) / FULL_RANGE) * swipe_sensitivity
}

fn is_snap_gesture(scrolling: &Scrolling, config: &Config) -> bool {
    config.snap_to_window()
        && config
            .swipe_gesture_fingers()
            .is_some_and(|fingers| scrolling.fingers_count == Some(fingers))
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn snap_trackpad_swipe(
    mut messages: MessageReader<Event>,
    active_workspace: Single<
        (Entity, &LayoutStrip, &Position, &mut Scrolling),
        With<ActiveWorkspaceMarker>,
    >,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    if !config.snap_to_window() {
        return;
    }

    let (strip_entity, strip, position, mut scrolling) = active_workspace.into_inner();
    if !is_snap_gesture(&scrolling, &config) {
        return;
    }

    let released = messages
        .read()
        .any(|event| matches!(event, Event::TouchpadUp));
    if !released && scrolling.last_event.elapsed() <= SNAP_GESTURE_TIMEOUT {
        return;
    }

    let viewport = active_display.actual_bounds(&config);
    let preferred = swipe_release_target(
        strip,
        position.0,
        scrolling.velocity,
        scrolling.started_focused,
        &viewport,
        &windows,
        &config,
    );
    let target = preferred
        .and_then(|entity| {
            centered_strip_position(entity, strip, position.0, &viewport, &windows, &config)
                .map(|position| (entity, position))
        })
        .or_else(|| {
            let entity = nearest_window(strip, position.0, &viewport, &windows)?;
            centered_strip_position(entity, strip, position.0, &viewport, &windows, &config)
                .map(|position| (entity, position))
        });

    if let Some((entity, target)) = target {
        scrolling.position = f64::from(target.x);
        commands.snap_entity_position(strip_entity, target);
        // Use the same raised native-focus path as keyboard focus movement
        // (Alt-H/Alt-L). This invokes the AX focus call even when the ECS
        // FocusedMarker already happens to be on the snapped window.
        commands.focus_entity(entity, true);
    }
    scrolling.velocity = 0.0;
    scrolling.is_user_swiping = false;
    scrolling.fingers_count = None;
    scrolling.started_focused = None;
    if let Ok(mut entity_commands) = commands.get_entity(strip_entity) {
        entity_commands.try_remove::<Scrolling>();
    }
}

fn swipe_release_target(
    strip: &LayoutStrip,
    strip_position: IVec2,
    velocity: f64,
    started_focused: Option<Entity>,
    viewport: &IRect,
    windows: &Windows,
    config: &Config,
) -> Option<Entity> {
    const MOMENTUM_SECONDS: f64 = 0.20;
    const FLING_VELOCITY_THRESHOLD: f64 = 2.2;

    let nearest = nearest_window(strip, strip_position, viewport, windows)?;
    if velocity.abs() < FLING_VELOCITY_THRESHOLD {
        return Some(nearest);
    }

    let Some(started_focused) = started_focused.filter(|entity| strip.contains(*entity)) else {
        return Some(nearest);
    };

    let direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    };
    let projected_x = f64::from(strip_position.x)
        + velocity * f64::from(viewport.width()) * direction_modifier * MOMENTUM_SECONDS;
    let projected_position = IVec2::new(projected_x.round() as i32, strip_position.y);

    let projected = nearest_window(strip, projected_position, viewport, windows)?;
    let start_index = strip.index_of(started_focused).ok()?;
    let projected_index = strip.index_of(projected).ok()?;
    let target = match projected_index.cmp(&start_index) {
        std::cmp::Ordering::Less => strip
            .left_neighbour(started_focused)
            .unwrap_or(started_focused),
        std::cmp::Ordering::Equal => started_focused,
        std::cmp::Ordering::Greater => strip
            .right_neighbour(started_focused)
            .unwrap_or(started_focused),
    };

    Some(target)
}

fn centered_strip_position(
    entity: Entity,
    strip: &LayoutStrip,
    current_position: IVec2,
    viewport: &IRect,
    windows: &Windows,
    config: &Config,
) -> Option<IVec2> {
    let layout = windows.layout_position(entity)?;
    let frame = windows.moving_frame(entity)?;
    let target_x = viewport.center().x - (layout.0.x + frame.width() / 2);
    let get_window_frame = |entity| windows.moving_frame(entity);
    let clamped_x = clamp_viewport_offset(
        target_x,
        strip,
        windows,
        &get_window_frame,
        viewport,
        config,
    )
    // Transient layout changes can make the full-strip clamp unavailable. The
    // selected window still has valid geometry, so center it instead of leaving
    // the strip at the arbitrary release position.
    .unwrap_or(target_x);

    Some(IVec2::new(clamped_x, current_position.y))
}

fn nearest_window(
    strip: &LayoutStrip,
    strip_position: IVec2,
    viewport: &IRect,
    windows: &Windows,
) -> Option<Entity> {
    let viewport_center = viewport.center().x;
    strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            let layout = windows.layout_position(entity)?;
            let frame = windows.moving_frame(entity)?;
            let center = layout.0.x + strip_position.x + frame.width() / 2;
            Some((
                entity,
                i64::from(center).abs_diff(i64::from(viewport_center)),
            ))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(entity, _)| entity)
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const MIN_VELOCITY_PX: f64 = 5.0;
    let dt = time.delta_secs_f64();
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, mut scroll) in strips {
        if scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD {
            if is_snap_gesture(&scroll, &config) {
                continue;
            }
            scroll.is_user_swiping = false;

            if scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<Scrolling>();
            }
            if let Some(point) = window_manager.cursor_position() {
                commands.trigger(SendMessageTrigger(Event::MouseMoved {
                    point,
                    modifiers: Modifiers::empty(),
                }));
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn apply_inertia(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    time: Res<Time>,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    for (_, mut scroll) in &mut strips {
        if scroll.is_user_swiping {
            continue;
        }

        if scroll.velocity.abs() > 0.001 {
            let decay_rate = config.swipe_deceleration();
            scroll.velocity *= (-decay_rate * dt).exp();
        } else {
            scroll.velocity = 0.0;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn apply_snap_force(
    mut strip: Single<(&LayoutStrip, &Position, &mut Scrolling)>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    time: Res<Time>,
) {
    const CENTER_MAGNETIC_FORCE: f64 = 10.0;
    const SNAP_DISPLAY_RATIO: f64 = 0.45;

    if !config.auto_center() {
        return;
    }

    let viewport = active_display.actual_bounds(&config);
    let viewport_center = viewport.center().x;
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    let (strip, position, ref mut scroll) = *strip;
    if scroll.is_user_swiping || scroll.velocity.abs() > 0.5 {
        return;
    }

    let target_offset = strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            windows
                .layout_position(entity)
                .map(|p| p.0.x)
                .zip(Some(entity))
        })
        .map(|(position, entity)| {
            let col_width = windows.moving_frame(entity).map_or(0, |f| f.width());
            viewport_center - (position + col_width / 2)
        })
        .min_by_key(|target| (position.x - target).abs())
        .unwrap_or(position.x);

    let dist_to_snap = f64::from(position.x - target_offset);
    if dist_to_snap.abs() < snap_threshold {
        let dt = time.delta_secs_f64();
        scroll.position -= dist_to_snap * dt * CENTER_MAGNETIC_FORCE;
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    let viewport = active_display.actual_bounds(&config);
    let viewport_width = f64::from(viewport.width());

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    };

    let scroll = &mut *strip;
    if scroll.velocity.abs() > 0.0001 {
        scroll.position += scroll.velocity * dt * viewport_width * direction_modifier;
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::TRACE, skip_all)]
fn apply_scrolling_constraints(
    mut strip: Single<
        (&LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
) {
    let viewport = active_display.actual_bounds(&config);
    let (strip, ref mut position, ref mut scroll) = *strip;

    let get_window_frame = |entity| windows.moving_frame(entity);
    if let Some(clamped_offset) = clamp_viewport_offset(
        scroll.position as i32,
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        &config,
    ) {
        position.x = clamped_offset;
        scroll.position = f64::from(clamped_offset);
    } else {
        scroll.velocity = 0.0;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn clamp_viewport_offset<W>(
    current_offset: i32,
    layout_strip: &LayoutStrip,
    windows: &Windows,
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let total_strip_width = layout_strip
        .last()
        .ok()
        .and_then(|column| column.top())
        .and_then(|entity| {
            windows
                .layout_position(entity)
                .zip(get_window_frame(entity))
        })
        .map(|(position, frame)| position.x + frame.width())?;

    let continuous_swipe = config.continuous_swipe();
    let strip_position = |column: PaneruResult<Column>| {
        column
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
    };

    let left_snap = strip_position(layout_strip.last());
    let right_snap = strip_position(layout_strip.get(1));

    Some(
        if continuous_swipe && let Some((left_snap, right_snap)) = left_snap.zip(right_snap) {
            // Allow to scroll away until the last or first window snaps.
            current_offset.clamp(viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

#[derive(Default)]
struct VerticalGestureState {
    accumulated: f64,
    last_event: Option<Instant>,
    fired: bool,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn vertical_swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
    mut state: Local<VerticalGestureState>,
) {
    const GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

    if active_display.fullscreen().is_some() {
        return;
    }

    // Reset state when the gesture times out (fingers lifted).
    if let Some(last) = state.last_event
        && last.elapsed() > GESTURE_TIMEOUT
    {
        state.accumulated = 0.0;
        state.fired = false;
    }

    for event in messages.read() {
        match event {
            Event::VerticalScrollTick { delta } => {
                switch_virtual_workspace(*delta, &config, &mut commands);
            }
            Event::VerticalSwipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                state.last_event = Some(Instant::now());

                if !state.fired {
                    state.accumulated += delta;
                }
            }
            _ => {}
        }
    }

    // Threshold needs to be high enough that incidental vertical movement
    // during horizontal swipes doesn't trigger a workspace switch.
    let threshold = 0.15 / config.swipe_sensitivity();
    if state.accumulated.abs() >= threshold {
        switch_virtual_workspace(state.accumulated, &config, &mut commands);
        state.accumulated = 0.0;
        state.fired = true;
    }
}

fn switch_virtual_workspace(delta: f64, config: &Config, commands: &mut Commands) {
    let physical_finger_direction = if delta > 0.0 {
        Direction::South
    } else {
        Direction::North
    };
    let direction = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => physical_finger_direction.reverse(),
        SwipeGestureDirection::Reversed => physical_finger_direction,
    };
    commands.trigger(SendMessageTrigger(Event::Command {
        command: Command::Window(Operation::Virtual(direction)),
    }));
}
