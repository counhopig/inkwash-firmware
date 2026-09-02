//! Full-screen todo and urgent-inbox reminders shared by every UI loop.
//! Persistence is updated before presentation so a reset or failed display
//! cannot trap the device in a repeated reminder loop.

use std::thread;
use std::time::Duration;

use esp_idf_svc::systime::EspSystemTime;

use crate::button::{ButtonEvent, POLL_INTERVAL_MS};
use crate::rtc::DateTime;
use crate::screens;
use crate::todos::Todo;
use crate::usb_console::reject_pending_command;
use crate::{ui, watchdog};

const URGENT_RING_MAX_SECS: u64 = 120;

/// Shows both reminder classes in priority order. Urgent inbox alerts go
/// first; a due-todo reminder may follow after the urgent alert is
/// dismissed. Returns a structured outcome so the caller can tell an RTC
/// alarm that preempted a reminder from an ordinary reminder dismissal.
/// An alarm short-circuits the chain: no further reminder class runs after
/// one.
pub fn poll(ctx: &mut crate::ctx::DeviceContext, now: &DateTime) -> crate::ctx::BackgroundOutcome {
    let urgent_outcome = remind_urgent_inbox(ctx);
    // An alarm short-circuits the chain: no further reminder class runs.
    if urgent_outcome == crate::ctx::BackgroundOutcome::AlarmHandled {
        return crate::ctx::BackgroundOutcome::AlarmHandled;
    }
    let todo_outcome = remind_due_todos(ctx, now);
    // Merge by stable priority so a plain urgent dismissal (VisibleChanged)
    // is not lost when no todo reminder ran.
    urgent_outcome.merge(todo_outcome)
}

fn remind_due_todos(
    ctx: &mut crate::ctx::DeviceContext,
    now: &DateTime,
) -> crate::ctx::BackgroundOutcome {
    use inkwash_logic::reminder_dedup::{
        already_reminded_today, due_high_importance_todos, reminder_date_key,
    };
    match ctx.counters.todo_reminded_date() {
        Ok(prev) if already_reminded_today(prev.as_deref(), now) => {
            return crate::ctx::BackgroundOutcome::NoChange;
        }
        Ok(_) => {}
        Err(err) => log::warn!("Failed to read todo reminder date: {err}"),
    }

    let Ok(list) = ctx.todo_store.load() else {
        return crate::ctx::BackgroundOutcome::NoChange;
    };
    let due: Vec<&Todo> = due_high_importance_todos(&list, now);
    if due.is_empty() {
        return crate::ctx::BackgroundOutcome::NoChange;
    }

    let date_key = reminder_date_key(now);
    if let Err(err) = ctx.counters.set_todo_reminded_date(&date_key) {
        log::warn!("Failed to record todo reminder date; not reminding: {err}");
        return crate::ctx::BackgroundOutcome::NoChange;
    }
    log::info!("{} high-importance todo(s) due today; reminding", due.len());
    show_due_todos(ctx, &due)
}

fn show_due_todos(
    ctx: &mut crate::ctx::DeviceContext,
    due: &[&Todo],
) -> crate::ctx::BackgroundOutcome {
    let mut canvas = ctx.board.display.canvas_mut();
    canvas.clear();
    ui::header(&mut canvas, "TODOS DUE");
    for (index, todo) in due.iter().take(7).enumerate() {
        let text = screens::truncate_prop(&todo.text, 300);
        canvas.draw_text_prop(16, 48 + index * 24, 1, &format!("!! {text}"));
    }
    if due.len() > 7 {
        canvas.draw_text_prop(16, 268, 1, "MORE...");
    }
    canvas.draw_text_prop(16, 284, 1, "ENTER = DISMISS");
    drop(canvas);
    ctx.board.display.refresh_full_best_effort();

    if let Some(audio) = ctx.board.audio.as_mut() {
        for _ in 0..3 {
            if let Err(err) = audio.play_sine_stereo(1046.0, 0.15, 8000) {
                log::warn!("Todo reminder tone failed: {err}");
                break;
            }
            thread::sleep(Duration::from_millis(150));
        }
    }
    loop {
        watchdog::feed();
        // An RTC alarm preempts the reminder: AppRunner rings over it and
        // the caller unwinds to main.
        if ctx.poll_alarm_snapshot() {
            ctx.app_runner_alarm_exit = true;
            return crate::ctx::BackgroundOutcome::AlarmHandled;
        }
        reject_pending_command(ctx.usb_console);
        if let Some(ble) = ctx.ble_control.as_mut() {
            crate::ble_control::reject_pending_command(ble);
        }
        if ctx
            .board
            .key_enter
            .poll()
            .is_some_and(|event| matches!(event, ButtonEvent::Pressed | ButtonEvent::LongPressed))
        {
            break;
        }
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
    }
    crate::ctx::BackgroundOutcome::VisibleChanged
}

fn remind_urgent_inbox(ctx: &mut crate::ctx::DeviceContext) -> crate::ctx::BackgroundOutcome {
    let Ok(list) = ctx.inbox_store.load() else {
        return crate::ctx::BackgroundOutcome::NoChange;
    };
    let urgent = ctx.inbox_store.unread_urgent().unwrap_or_default();
    if urgent.is_empty() {
        return crate::ctx::BackgroundOutcome::NoChange;
    }
    for id in &urgent {
        if let Err(err) = ctx.inbox_store.mark_read(*id) {
            log::warn!("Failed to mark urgent inbox read; not reminding: {err}");
            return crate::ctx::BackgroundOutcome::NoChange;
        }
    }
    let titles: Vec<String> = list
        .iter()
        .filter(|item| urgent.contains(&item.id))
        .map(|item| screens::truncate_prop(&item.title, 330))
        .collect();
    log::info!("{} urgent inbox message(s) to show", titles.len());
    show_urgent(ctx, &titles)
}

fn show_urgent(
    ctx: &mut crate::ctx::DeviceContext,
    titles: &[String],
) -> crate::ctx::BackgroundOutcome {
    let mut canvas = ctx.board.display.canvas_mut();
    canvas.clear();
    ui::header(&mut canvas, "URGENT");
    for (index, title) in titles.iter().take(4).enumerate() {
        canvas.draw_text_prop(16, 48 + index * 24, 1, &format!("!! {title}"));
    }
    if titles.len() > 4 {
        canvas.draw_text_prop(16, 268, 1, "MORE IN INBOX...");
    }
    canvas.draw_text_prop(16, 284, 1, "ENTER = DISMISS");
    drop(canvas);
    ctx.board.display.refresh_full_best_effort();

    if let Some(audio) = ctx.board.audio.as_mut() {
        if let Err(err) = audio.set_volume(255) {
            log::warn!("Urgent volume boost failed: {err}");
        }
    }
    const SIREN: [(f32, f32); 2] = [(1397.0, 0.12), (1046.0, 0.12)];
    let mut siren_step = 0usize;
    let ring_start = EspSystemTime {}.now();
    loop {
        watchdog::feed();
        // An RTC alarm preempts the urgent reminder (see show_due_todos).
        if ctx.poll_alarm_snapshot() {
            ctx.app_runner_alarm_exit = true;
            return crate::ctx::BackgroundOutcome::AlarmHandled;
        }
        reject_pending_command(ctx.usb_console);
        if let Some(ble) = ctx.ble_control.as_mut() {
            crate::ble_control::reject_pending_command(ble);
        }
        ctx.board.key_enter.poll();
        if ctx.board.key_enter.is_pressed() {
            while ctx.board.key_enter.poll().is_some() {}
            return crate::ctx::BackgroundOutcome::VisibleChanged;
        }
        if (EspSystemTime {}).now().saturating_sub(ring_start)
            >= Duration::from_secs(URGENT_RING_MAX_SECS)
        {
            log::warn!("Urgent reminder timed out after {URGENT_RING_MAX_SECS}s");
            return crate::ctx::BackgroundOutcome::VisibleChanged;
        }
        let (frequency, duration) = SIREN[siren_step % SIREN.len()];
        if let Some(audio) = ctx.board.audio.as_mut() {
            if let Err(err) = audio.play_sine_stereo(frequency, duration, 24000) {
                log::warn!("Urgent siren note failed: {err}");
            }
        } else {
            thread::sleep(Duration::from_millis(duration as u64 * 1000));
        }
        siren_step += 1;
        let poll_deadline = EspSystemTime {}.now() + Duration::from_millis(400);
        loop {
            watchdog::feed();
            if ctx.poll_alarm_snapshot() {
                ctx.app_runner_alarm_exit = true;
                return crate::ctx::BackgroundOutcome::AlarmHandled;
            }
            reject_pending_command(ctx.usb_console);
            if let Some(ble) = ctx.ble_control.as_mut() {
                crate::ble_control::reject_pending_command(ble);
            }
            ctx.board.key_enter.poll();
            if ctx.board.key_enter.is_pressed() {
                while ctx.board.key_enter.poll().is_some() {}
                return crate::ctx::BackgroundOutcome::VisibleChanged;
            }
            if (EspSystemTime {}).now() >= poll_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
    }
}
