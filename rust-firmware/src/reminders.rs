//! Reminder fact layer (Stage 5/6).
//!
//! Gathers whether a full-screen reminder is due (urgent inbox alerts first,
//! then high-importance todos due today) and, when one is, dispatches
//! `Event::ReminderDue` into the state machine with the *final* visible
//! lines. Persistence is applied here, before dispatch, so a reset or a
//! dropped dispatch cannot trap the device in a repeated reminder loop -
//! the same "persist before present" guarantee the legacy blocking loops
//! had.
//!
//! Nothing blocks and nothing is drawn here: the SM owns the reminder
//! overlay (Screen::Reminder), the RenderPlan-driven executor renders it
//! (Effect::Render -> ViewModel -> RenderPlan -> EPD), and any button /
//! the deadline Tick dismisses it through the SM. The legacy full-screen
//! show loops and their direct `refresh_full_best_effort` calls are gone.

use crate::rtc::DateTime;

/// The urgent-reminder / due-todo cadence is the caller's choice (main runs
/// this once per scheduled-sync boundary). Returns nothing that implies a
/// redraw happened; the SM's own render covers the overlay.
pub fn poll(ctx: &mut crate::ctx::DeviceContext, _now: &DateTime) {
    // Never stack a reminder over one that is already showing (an alarm is
    // ringing or a reminder is current): the SM's transition_reminder_due
    // also drops reminders while an alarm rings.
    use inkwash_logic::app::Screen;
    let screen_blocks = {
        let runner = ctx.app_runner.borrow();
        let state = runner.state();
        matches!(state.screen, Screen::Reminder(_) | Screen::AlarmRinging)
    };
    if screen_blocks {
        return;
    }

    // Urgent inbox first (priority over todo). An urgent reminder being
    // shown preempts the todo class in the same pass: once an urgent
    // ReminderDue has been dispatched the SM is on Screen::Reminder, and
    // the next poll pass sees screen_blocks and stops - so urgent is shown
    // and todo waits until the urgent is dismissed (the SM dismiss then
    // allows a later pass to raise the todo reminder). This is the
    // urgent>todo ordering with no alarm/todo stacking.
    let now = _now;
    if let Some(payload) = urgent_payload(ctx) {
        dispatch(ctx, payload);
        return;
    }
    if let Some(payload) = todo_payload(ctx, now) {
        dispatch(ctx, payload);
    }
}

fn dispatch(ctx: &mut crate::ctx::DeviceContext, payload: inkwash_logic::app::ReminderPayload) {
    if let Err(err) = ctx.dispatch_event(inkwash_logic::app::Event::ReminderDue(payload)) {
        log::warn!("ReminderDue dispatch failed: {err}");
    }
}

/// Gathers urgent inbox titles, marking each read first (persist-before-
/// present), and returns the payload when there was something urgent.
fn urgent_payload(
    ctx: &mut crate::ctx::DeviceContext,
) -> Option<inkwash_logic::app::ReminderPayload> {
    use inkwash_logic::app::{ReminderKind, ReminderPayload};
    let list = ctx.inbox_store.load().ok()?;
    let urgent = ctx.inbox_store.unread_urgent().ok()?;
    if urgent.is_empty() {
        return None;
    }
    for id in &urgent {
        if let Err(err) = ctx.inbox_store.mark_read(*id) {
            log::warn!("Failed to mark urgent inbox read; not reminding: {err}");
            return None;
        }
    }
    let lines: Vec<String> = list
        .iter()
        .filter(|item| urgent.contains(&item.id))
        .map(|item| crate::screens::truncate_prop(&item.title, 330))
        .collect();
    if lines.is_empty() {
        return None;
    }
    log::info!("{} urgent inbox message(s) to remind", lines.len());
    Some(ReminderPayload {
        kind: ReminderKind::Urgent,
        lines,
    })
}

/// Gathers high-importance todos due today, recording the reminder date
/// first (persist-before-present), and returns the payload when there was
/// something due.
fn todo_payload(
    ctx: &mut crate::ctx::DeviceContext,
    now: &DateTime,
) -> Option<inkwash_logic::app::ReminderPayload> {
    use inkwash_logic::app::{ReminderKind, ReminderPayload};
    use inkwash_logic::reminder_dedup::{
        already_reminded_today, due_high_importance_todos, reminder_date_key,
    };
    match ctx.counters.todo_reminded_date() {
        Ok(prev) if already_reminded_today(prev.as_deref(), now) => return None,
        Ok(_) => {}
        Err(err) => log::warn!("Failed to read todo reminder date: {err}"),
    }
    let list = ctx.todo_store.load().ok()?;
    let due = due_high_importance_todos(&list, now);
    if due.is_empty() {
        return None;
    }
    let date_key = reminder_date_key(now);
    if let Err(err) = ctx.counters.set_todo_reminded_date(&date_key) {
        log::warn!("Failed to record todo reminder date; not reminding: {err}");
        return None;
    }
    let lines: Vec<String> = due
        .iter()
        .map(|t| crate::screens::truncate_prop(&t.text, 300))
        .collect();
    log::info!(
        "{} high-importance todo(s) due today; reminding",
        lines.len()
    );
    Some(ReminderPayload {
        kind: ReminderKind::Todo,
        lines,
    })
}
