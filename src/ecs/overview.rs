use bevy::app::{App, Plugin, PreUpdate};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, FocusedMarker, LayoutPosition,
    SpawnCommandsExt,
};
use crate::events::Event;
use crate::manager::{Application, Display, Window};
use crate::overlay::{OverviewItem, OverviewManager, OverviewRow};
use crate::platform::input::{activate_jump_picker, deactivate_jump_picker};

pub struct OverviewPlugin;

#[derive(Default, Resource)]
struct OverviewState {
    targets: Vec<Entity>,
}

impl Plugin for OverviewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OverviewState>()
            .add_systems(PreUpdate, handle_overview);
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn handle_overview(
    mut messages: MessageReader<Event>,
    config: Res<Config>,
    mut state: ResMut<OverviewState>,
    strips: Query<(&LayoutStrip, &ChildOf, Has<ActiveWorkspaceMarker>)>,
    displays: Query<(Entity, &Display, Has<ActiveDisplayMarker>)>,
    windows: Query<(&LayoutPosition, &Bounds, Has<FocusedMarker>, &ChildOf), With<Window>>,
    applications: Query<&Application>,
    mut overview: Option<NonSendMut<OverviewManager>>,
    mut focus_history: ResMut<FocusHistory>,
    mut commands: Commands,
) {
    let mut open = false;
    let mut selection = None;
    let mut cancel = false;
    for event in messages.read() {
        match event {
            Event::Command {
                command: Command::Jump,
            } => open = true,
            Event::JumpPickerSelect { index } => selection = Some(*index),
            Event::JumpPickerCancel => cancel = true,
            _ => {}
        }
    }

    if let Some(index) = selection {
        let target = state.targets.get(index).copied();
        close_overview(&mut state, &mut overview);
        if let Some(target) = target {
            focus_history.pending_focus = Some(target);
            commands.focus_entity(target, true);
            commands.reshuffle_around(target);
        }
        return;
    }

    if cancel {
        close_overview(&mut state, &mut overview);
        return;
    }
    if !open || !config.jump_picker_enabled() {
        return;
    }

    let Some((active_display_entity, active_display, _)) =
        displays.iter().find(|(_, _, active)| *active)
    else {
        return;
    };

    let mut rows = strips
        .iter()
        .filter(|(_, child, _)| child.parent() == active_display_entity)
        .collect::<Vec<_>>();
    rows.sort_by_key(|(strip, _, _)| (strip.id(), strip.virtual_index));
    if rows.is_empty() {
        return;
    }

    let keys = config.jump_picker_keys();
    if keys.is_empty() {
        return;
    }
    let display = active_display.bounds();
    let canvas_w = f64::from(display.width());
    let canvas_h = f64::from(display.height());
    let margin = 28.0;
    let row_gap = 10.0;
    let row_h = ((canvas_h - 2.0 * margin - row_gap * (rows.len() - 1) as f64) / rows.len() as f64)
        .max(44.0);

    let mut visual_rows = Vec::new();
    let mut visual_items = Vec::new();
    let mut targets = Vec::new();

    for (row_index, (strip, _, active)) in rows.into_iter().enumerate() {
        let row_y = canvas_h - margin - row_h - row_index as f64 * (row_h + row_gap);
        let row_frame = NSRect::new(
            NSPoint::new(margin, row_y),
            NSSize::new(canvas_w - 2.0 * margin, row_h),
        );
        visual_rows.push(OverviewRow {
            frame: row_frame,
            label: format!(
                "Space {} · workspace {}{}",
                strip.id(),
                strip.virtual_index + 1,
                if active { " · active" } else { "" }
            ),
        });

        let members = strip
            .all_windows()
            .into_iter()
            .filter_map(|entity| {
                windows
                    .get(entity)
                    .ok()
                    .map(|(position, bounds, focused, child)| {
                        let pid = applications
                            .get(child.parent())
                            .map_or(0, |application| application.pid());
                        (entity, position.0, bounds.0, focused, pid)
                    })
            })
            .collect::<Vec<_>>();
        let logical_w = members
            .iter()
            .map(|(_, position, size, _, _)| position.x + size.x)
            .max()
            .unwrap_or(1)
            .max(1);
        let logical_h = members
            .iter()
            .map(|(_, position, size, _, _)| position.y + size.y)
            .max()
            .unwrap_or(1)
            .max(1);
        let content = NSRect::new(
            NSPoint::new(row_frame.origin.x + 10.0, row_frame.origin.y + 8.0),
            NSSize::new(
                row_frame.size.width - 20.0,
                (row_frame.size.height - 34.0).max(12.0),
            ),
        );
        let scale_x = content.size.width / f64::from(logical_w);
        let scale_y = content.size.height / f64::from(logical_h);

        for (entity, position, size, focused, pid) in &members {
            if targets.len() >= keys.len() {
                break;
            }
            let item_frame = NSRect::new(
                NSPoint::new(
                    content.origin.x + f64::from(position.x) * scale_x + 2.0,
                    content.origin.y + content.size.height
                        - f64::from(position.y + size.y) * scale_y
                        + 2.0,
                ),
                NSSize::new(
                    (f64::from(size.x) * scale_x - 4.0).max(5.0),
                    (f64::from(size.y) * scale_y - 4.0).max(5.0),
                ),
            );
            visual_items.push(OverviewItem {
                frame: item_frame,
                mark: keys[targets.len()].0.to_string(),
                focused: *focused,
                pid: *pid,
            });
            targets.push(*entity);
        }
    }

    if targets.is_empty() {
        return;
    }
    activate_jump_picker(
        keys.iter()
            .take(targets.len())
            .map(|(_, code)| *code)
            .collect(),
    );
    state.targets = targets;

    if let Some(ref mut overview) = overview {
        overview.show(
            NSRect::new(
                NSPoint::new(f64::from(display.min.x), f64::from(display.min.y)),
                NSSize::new(canvas_w, canvas_h),
            ),
            visual_rows,
            visual_items,
        );
    }
}

fn close_overview(state: &mut OverviewState, overview: &mut Option<NonSendMut<OverviewManager>>) {
    deactivate_jump_picker();
    state.targets.clear();
    if let Some(overview) = overview {
        overview.remove();
    }
}
