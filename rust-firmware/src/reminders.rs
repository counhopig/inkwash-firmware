//! Full-screen todo and urgent-inbox reminders shared by every UI loop.
//! Persistence is updated before presentation so a reset or failed display
//! cannot trap the device in a repeated reminder loop.

use std::thread;
use std::time::Duration;

use esp_idf_svc::systime::EspSystemTime;

use crate::ble_control::BleControl;
use crate::board::Note4Board;
use crate::button::{ButtonEvent, POLL_INTERVAL_MS};
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::screens;
use crate::storage::PersistedCounters;
use crate::todos::{Todo, TodoStore};
use crate::usb_console::{reject_pending_command, UsbConsole};
use crate::{ui, watchdog};

const URGENT_RING_MAX_SECS: u64 = 120;

/// Shows both reminder classes in priority order. Urgent inbox alerts go
/// first; a due-todo reminder may follow after the urgent alert is dismissed.
pub fn poll(
    board: &mut Note4Board,
    counters: &PersistedCounters,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    usb: &mut UsbConsole,
    mut ble: Option<&mut BleControl>,
    now: &DateTime,
) -> bool {
    let urgent_shown = remind_urgent_inbox(board, inbox_store, usb, ble.as_deref_mut());
    let todo_shown = remind_due_todos(board, counters, todo_store, usb, ble, now);
    urgent_shown || todo_shown
}

fn remind_due_todos(
    board: &mut Note4Board,
    counters: &PersistedCounters,
    todo_store: &TodoStore,
    usb: &mut UsbConsole,
    ble: Option<&mut BleControl>,
    now: &DateTime,
) -> bool {
    use inkwash_logic::reminder_dedup::{
        already_reminded_today, due_high_importance_todos, reminder_date_key,
    };
    match counters.todo_reminded_date() {
        Ok(prev) if already_reminded_today(prev.as_deref(), now) => return false,
        Ok(_) => {}
        Err(err) => log::warn!("Failed to read todo reminder date: {err}"),
    }

    let Ok(list) = todo_store.load() else {
        return false;
    };
    let due: Vec<&Todo> = due_high_importance_todos(&list, now);
    if due.is_empty() {
        return false;
    }

    let date_key = reminder_date_key(now);
    if let Err(err) = counters.set_todo_reminded_date(&date_key) {
        log::warn!("Failed to record todo reminder date; not reminding: {err}");
        return false;
    }
    log::info!("{} high-importance todo(s) due today; reminding", due.len());
    show_due_todos(board, &due, usb, ble);
    true
}

fn show_due_todos(
    board: &mut Note4Board,
    due: &[&Todo],
    usb: &mut UsbConsole,
    mut ble: Option<&mut BleControl>,
) {
    let mut canvas = board.display.canvas_mut();
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
    board.display.refresh_full_best_effort();

    if let Some(audio) = board.audio.as_mut() {
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
        reject_pending_command(usb);
        if let Some(ble) = ble.as_deref_mut() {
            crate::ble_control::reject_pending_command(ble);
        }
        if board
            .key_enter
            .poll()
            .is_some_and(|event| matches!(event, ButtonEvent::Pressed | ButtonEvent::LongPressed))
        {
            break;
        }
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
    }
}

fn remind_urgent_inbox(
    board: &mut Note4Board,
    inbox_store: &InboxStore,
    usb: &mut UsbConsole,
    ble: Option<&mut BleControl>,
) -> bool {
    let Ok(list) = inbox_store.load() else {
        return false;
    };
    let urgent = inbox_store.unread_urgent().unwrap_or_default();
    if urgent.is_empty() {
        return false;
    }
    for id in &urgent {
        if let Err(err) = inbox_store.mark_read(*id) {
            log::warn!("Failed to mark urgent inbox read; not reminding: {err}");
            return false;
        }
    }
    let titles: Vec<String> = list
        .iter()
        .filter(|item| urgent.contains(&item.id))
        .map(|item| screens::truncate_prop(&item.title, 330))
        .collect();
    log::info!("{} urgent inbox message(s) to show", titles.len());
    show_urgent(board, &titles, usb, ble);
    true
}

fn show_urgent(
    board: &mut Note4Board,
    titles: &[String],
    usb: &mut UsbConsole,
    mut ble: Option<&mut BleControl>,
) {
    let mut canvas = board.display.canvas_mut();
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
    board.display.refresh_full_best_effort();

    if let Some(audio) = board.audio.as_mut() {
        if let Err(err) = audio.set_volume(255) {
            log::warn!("Urgent volume boost failed: {err}");
        }
    }
    const SIREN: [(f32, f32); 2] = [(1397.0, 0.12), (1046.0, 0.12)];
    let mut siren_step = 0usize;
    let ring_start = EspSystemTime {}.now();
    loop {
        watchdog::feed();
        reject_pending_command(usb);
        if let Some(ble) = ble.as_deref_mut() {
            crate::ble_control::reject_pending_command(ble);
        }
        board.key_enter.poll();
        if board.key_enter.is_pressed() {
            while board.key_enter.poll().is_some() {}
            return;
        }
        if (EspSystemTime {}).now().saturating_sub(ring_start)
            >= Duration::from_secs(URGENT_RING_MAX_SECS)
        {
            log::warn!("Urgent reminder timed out after {URGENT_RING_MAX_SECS}s");
            return;
        }
        let (frequency, duration) = SIREN[siren_step % SIREN.len()];
        if let Some(audio) = board.audio.as_mut() {
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
            reject_pending_command(usb);
            if let Some(ble) = ble.as_deref_mut() {
                crate::ble_control::reject_pending_command(ble);
            }
            board.key_enter.poll();
            if board.key_enter.is_pressed() {
                while board.key_enter.poll().is_some() {}
                return;
            }
            if (EspSystemTime {}).now() >= poll_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
    }
}
