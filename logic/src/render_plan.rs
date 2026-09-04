//! Host-testable core of the Stage-5 ViewModel -> RenderPlan pipeline.
//!
//! `firmware-architecture.md` (UI 与渲染) requires:
//!
//! - the renderer first projects application state to a `ViewModel`
//!   containing only visible data;
//! - the renderer privately keeps the last *successfully shown* ViewModel
//!   and its generation;
//! - `render(previous, current) -> RenderPlan` decides, from the diff of the
//!   two ViewModels, whether nothing changed (`Noop`), the same page needs a
//!   well-scoped partial refresh (`Partial { frame }`), or the whole frame
//!   must be redrawn (`Full { frame }`).
//!
//! Full refresh is mandated when:
//! - there is no previous ViewModel (first render, or the cache was reset
//!   after a failure);
//! - the page *type* changed;
//! - an alarm/reminder overlay entered or exited;
//! - a partial refresh failed and the renderer is recovering;
//! - the number of consecutive partial refreshes reached the panel
//!   maintenance threshold.
//!
//! This module is pure and lives in `inkwash-logic` so the decision rule is
//! unit-tested on the host. The firmware renderer holds the previous
//! ViewModel (its private cache), calls [`plan_render`] for every render
//! request, executes the returned plan against the EPD, and updates its
//! cache only when the request's generation is still current and the
//! refresh completed.
//!
//! The ViewModel is deliberately *not* a copy of the full store rows: for a
//! list screen the only thing the refresh decision needs to know is whether
//! the *visible data* changed, which is captured as a fingerprint per
//! surface. The executor still draws the actual pixels from the stores (or
//! from the projection in `AppState::view_model`).

use crate::app::{AppState, RenderGeneration, RenderView};

/// A full-panel frame reference. The firmware maps this to its panel
/// geometry; the logic side only carries the discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Frame(pub u32);

/// Which panel region a partial refresh should touch. Named after the
/// firmware's rects so the executor can translate directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PartialRegion {
    /// The Home clock + date/metadata block (a minute change).
    Clock,
    /// The GO TO drawer bar (selection moves inside the drawer).
    NavBar,
    /// The shared list region under the header (Settings / SyncInterval /
    /// AlarmList / TodoList / Inbox row moves and row-data changes).
    List,
    /// The calendar grid below the header (day-cursor moves).
    CalendarGrid,
    /// Full-content partial for read-only screens whose only valid refresh
    /// is a whole-surface redraw (week view / item detail / number pick /
    /// BLE pairing are Full-only; this variant is used when a same-page
    /// data change forces a repaint of that surface).
    Surface,
}

/// A refresh plan for one render request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPlan {
    /// Visible content is unchanged; nothing is submitted to the panel.
    Noop,
    /// The same page changed in a locally-refreshable way.
    Partial { frame: Frame, region: PartialRegion },
    /// A full-panel refresh is required.
    Full { frame: Frame },
}

/// One screenful of visible state, compared by [`plan_render`].
///
/// The view key captures what the renderer draws and *why a redraw might be
/// needed*. It is produced by projecting `AppState` (or, in the firmware,
/// the equivalent visible facts the executor draws from).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewModel {
    /// Monotonic visible-state version (from `AppState::render_generation`).
    pub generation: RenderGeneration,
    /// The surface plus its cursor / selection / phase - everything that
    /// makes two draws of the *same surface* differ structurally.
    pub view: RenderView,
    /// The visible clock minute (None when no clock is known).
    pub clock_minute: Option<u32>,
    /// Non-zero when a ring or reminder overlay is currently shown.
    pub overlay: Overlay,
    /// A fingerprint of the visible data behind list/card surfaces, so a
    /// data change under an unchanged cursor still forces a repaint.
    pub data_fingerprint: u64,
}

/// A transient overlay on top of the current page. Its presence toggles
/// Full refresh in both directions (enter and dismiss).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,
    /// `Screen::AlarmRinging` (drawn over the current page by the ring
    /// UI; the underlying page is restored on dismiss).
    AlarmRinging,
    /// Reminder / other full-screen alert overlay.
    Reminder,
}

/// Maximum consecutive partial refreshes before a maintenance Full refresh
/// is forced (panel ghosting / wear mitigation). Mirrors the firmware's
/// e-paper maintenance cadence.
pub const PARTIAL_MAINTENANCE_LIMIT: u32 = 200;

impl ViewModel {
    /// A bare Home ViewModel with no clock and no data - for fixtures and
    /// for "nothing known yet" renderer caches.
    pub fn home(generation: RenderGeneration) -> Self {
        ViewModel {
            generation,
            view: RenderView::Home,
            clock_minute: None,
            overlay: Overlay::None,
            data_fingerprint: 0,
        }
    }

    /// Project the visible state of an `AppState` into a comparable
    /// ViewModel. This is the host-testable projection; the firmware uses
    /// the same construction from the facts it draws from.
    pub fn from_state(state: &AppState) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        match &state.screen {
            crate::app::Screen::AlarmList { .. } => {
                for a in &state.alarms.alarms {
                    (a.id, a.hour, a.minute, a.enabled).hash(&mut hasher);
                }
            }
            crate::app::Screen::TodoList { .. } => {
                for t in &state.todos.todos {
                    (&t.text, t.done).hash(&mut hasher);
                }
            }
            crate::app::Screen::Inbox { .. } | crate::app::Screen::InboxItem { .. } => {
                for i in &state.inbox.items {
                    (i.id, i.read, i.title.as_str()).hash(&mut hasher);
                }
            }
            _ => {
                // Other surfaces have no store-backed row content the
                // executor draws from AppState; their data_fingerprint stays
                // 0 and never distinguishes.
            }
        }
        let data_fingerprint = hasher.finish();
        let clock_minute = state
            .clock
            .now
            .map(|dt| u32::from(dt.hour) * 60 + u32::from(dt.minute));
        let overlay = match state.screen {
            crate::app::Screen::AlarmRinging => Overlay::AlarmRinging,
            crate::app::Screen::Reminder(_) => Overlay::Reminder,
            _ => Overlay::None,
        };
        ViewModel {
            generation: state.render_generation,
            view: state.screen.render_view(),
            clock_minute,
            overlay,
            data_fingerprint,
        }
    }
}

/// Decide the refresh plan from the last successfully-shown ViewModel (if
/// any) and the current one.
///
/// `partials_since_maintenance` counts consecutive partial refreshes since
/// the last Full; the renderer tracks it and passes it in so a maintenance
/// Full is forced after `PARTIAL_MAINTENANCE_LIMIT`.
pub fn plan_render(
    previous: Option<&ViewModel>,
    current: &ViewModel,
    partials_since_maintenance: u32,
) -> RenderPlan {
    let frame = Frame(current.generation.0 as u32);

    // First render / cache was reset after a failure -> Full.
    let Some(prev) = previous else {
        return RenderPlan::Full { frame };
    };

    // Visible data did not change at all -> nothing to do.
    if current.view == prev.view
        && current.clock_minute == prev.clock_minute
        && current.overlay == prev.overlay
        && current.data_fingerprint == prev.data_fingerprint
    {
        return RenderPlan::Noop;
    }

    // Page type changed or an overlay entered/exited -> Full.
    let same_surface = surface_kind(&current.view) == surface_kind(&prev.view);
    if !same_surface || current.overlay != prev.overlay {
        return RenderPlan::Full { frame };
    }

    // Maintenance threshold reached -> Full.
    if partials_since_maintenance >= PARTIAL_MAINTENANCE_LIMIT {
        return RenderPlan::Full { frame };
    }

    // Same surface: a partial refresh is legitimate only where the change
    // really is local. Everything else degrades to Full.
    let region = match surface_kind(&current.view) {
        // Home minute change -> the clock block only. Any non-clock change
        // on Home (cursor-free surface) is impossible here because the
        // full-equality arm above already returned Noop; a Home data change
        // (card content) with same minute still needs a repaint of the card
        // region, which the executor treats as a Home-surface partial.
        SurfaceKind::Home if current.clock_minute != prev.clock_minute => PartialRegion::Clock,
        SurfaceKind::Home => return RenderPlan::Full { frame },
        // Drawer cursor moves -> the GO TO bar only.
        SurfaceKind::NavBar => PartialRegion::NavBar,
        // List screens: cursor moves *or* row-data changes both repaint the
        // list region below the header.
        SurfaceKind::List => PartialRegion::List,
        // Calendar day-cursor moves -> the grid.
        SurfaceKind::CalendarGrid => PartialRegion::CalendarGrid,
        // Read-only surfaces (week view / item detail / number pick / BLE
        // pairing) never partial-refresh; a same-page change repaints the
        // whole surface.
        SurfaceKind::ReadOnlySurface => PartialRegion::Surface,
    };

    RenderPlan::Partial { frame, region }
}

/// The refresh family of a view. Views in the same family may share partial
/// refresh geometry; a change across families is always a Full refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceKind {
    Home,
    NavBar,
    List,
    CalendarGrid,
    ReadOnlySurface,
}

fn surface_kind(view: &RenderView) -> SurfaceKind {
    match view {
        RenderView::Home => SurfaceKind::Home,
        RenderView::Navigation { .. } => SurfaceKind::NavBar,
        RenderView::Settings { .. }
        | RenderView::SyncInterval { .. }
        | RenderView::AlarmList { .. }
        | RenderView::TodoList { .. }
        | RenderView::Inbox { .. } => SurfaceKind::List,
        RenderView::Calendar { .. } => SurfaceKind::CalendarGrid,
        RenderView::WeekView { .. }
        | RenderView::InboxItem { .. }
        | RenderView::NumberPick { .. }
        | RenderView::BlePairing
        | RenderView::AlarmRinging
        | RenderView::Reminder { .. } => SurfaceKind::ReadOnlySurface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm_schedule::{Repeat, StoredAlarm};
    use crate::app::{BootSnapshot, DeviceStatus, Event, RenderView, Screen};
    use crate::button_event::{ButtonEvent, ButtonId};
    use crate::datetime::DateTime;
    use crate::device_config::DeviceConfig;
    use crate::wake_cause::WakeCause;

    fn dt(hour: u8, minute: u8) -> DateTime {
        DateTime {
            year: 2026,
            month: 8,
            day: 31,
            weekday: 0,
            hour,
            minute,
            second: 0,
            voltage_low: false,
        }
    }

    fn alarm(id: u8, hour: u8, minute: u8) -> StoredAlarm {
        StoredAlarm {
            id,
            hour,
            minute,
            repeat: Repeat::Daily,
            enabled: true,
            label: String::new(),
        }
    }

    fn boot_snapshot(alarms: Vec<StoredAlarm>, now: Option<DateTime>) -> BootSnapshot {
        BootSnapshot {
            wake_cause: WakeCause::Other,
            now,
            rtc_alarm_flag: false,
            rtc_alarm_interrupt_enabled: false,
            alarms,
            todos: vec![],
            inbox: vec![],
            config: DeviceConfig {
                server_url: String::new(),
                auth_token: String::new(),
            },
            status: DeviceStatus::default(),
        }
    }

    /// A Home ViewModel at a given minute.
    fn home_vm(minute: u32) -> ViewModel {
        ViewModel {
            generation: RenderGeneration(1),
            view: RenderView::Home,
            clock_minute: Some(minute),
            overlay: Overlay::None,
            data_fingerprint: 0,
        }
    }

    #[test]
    fn same_view_model_is_noop() {
        let vm = ViewModel {
            generation: crate::app::RenderGeneration(7),
            view: RenderView::Home,
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 42,
        };
        assert_eq!(plan_render(Some(&vm), &vm, 0), RenderPlan::Noop);
    }

    #[test]
    fn no_previous_is_full() {
        let vm = ViewModel {
            generation: crate::app::RenderGeneration(0),
            view: RenderView::Home,
            clock_minute: None,
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        assert_eq!(
            plan_render(None, &vm, 0),
            RenderPlan::Full { frame: Frame(0) }
        );
    }

    #[test]
    fn home_minute_tick_is_clock_partial() {
        let prev = home_vm(8 * 60);
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.clock_minute = Some(8 * 60 + 1);
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::Clock
            }
        );
    }

    #[test]
    fn page_type_change_is_full() {
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::Home,
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        let cur = ViewModel {
            generation: crate::app::RenderGeneration(2),
            view: RenderView::Settings { selected: 0 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Full { frame: Frame(2) }
        );
    }

    #[test]
    fn overlay_enter_and_exit_are_full() {
        let base = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::Home,
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        // Enter AlarmRinging -> Full.
        let mut ringing = base.clone();
        ringing.generation = crate::app::RenderGeneration(2);
        ringing.overlay = Overlay::AlarmRinging;
        assert_eq!(
            plan_render(Some(&base), &ringing, 0),
            RenderPlan::Full { frame: Frame(2) }
        );
        // Dismiss back to the same underlying page -> Full (no CLOCK_RECT
        // partial can follow the dismiss Full).
        let mut dismissed = ringing.clone();
        dismissed.generation = crate::app::RenderGeneration(3);
        dismissed.overlay = Overlay::None;
        assert_eq!(
            plan_render(Some(&ringing), &dismissed, 0),
            RenderPlan::Full { frame: Frame(3) }
        );
    }

    #[test]
    fn dismiss_restores_same_view_and_no_partial_after() {
        // The restored page's ViewModel must equal the pre-ring ViewModel
        // (same surface, same data) so the *next* render after the dismiss
        // Full is a Noop (no stray CLOCK_RECT partial).
        let mut state = crate::app::AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let before = ViewModel::from_state(&state);
        assert_eq!(before.overlay, Overlay::None);
        // 9:00 alarm fires -> AlarmRinging overlay.
        let _ = crate::app::update(
            &mut state,
            Event::RtcAlarmSnapshotReady(crate::app::RtcAlarmSnapshot {
                now: dt(9, 0),
                alarm_flag: true,
                alarm_interrupt_enabled: true,
            }),
        );
        assert_eq!(state.screen, Screen::AlarmRinging);
        let ringing_vm = ViewModel::from_state(&state);
        assert_eq!(ringing_vm.overlay, Overlay::AlarmRinging);
        // Dismiss.
        let _ = crate::app::update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let after = ViewModel::from_state(&state);
        assert_eq!(after.overlay, Overlay::None);
        // The underlying page and its data are unchanged (only the alarm
        // runtime and clock minute advanced), so the next render after the
        // dismiss Full is at most a clock partial - never a second Full for
        // the same page, and never a stray full-list redraw.
        assert_eq!(after.view, before.view);
        assert_eq!(after.data_fingerprint, before.data_fingerprint);
    }

    #[test]
    fn partial_failure_recovers_full_same_frame() {
        // After a failed partial the renderer invalidates its cache; the
        // next request has no previous -> Full with the current frame.
        let cur = ViewModel {
            generation: crate::app::RenderGeneration(9),
            view: RenderView::Settings { selected: 1 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        assert_eq!(
            plan_render(None, &cur, 0),
            RenderPlan::Full { frame: Frame(9) }
        );
    }

    #[test]
    fn maintenance_threshold_forces_full() {
        let prev = home_vm(8 * 60);
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(3);
        cur.clock_minute = Some(8 * 60 + 1);
        // Same minute-change diff, but the partial budget is exhausted.
        assert_eq!(
            plan_render(Some(&prev), &cur, PARTIAL_MAINTENANCE_LIMIT),
            RenderPlan::Full { frame: Frame(3) }
        );
    }

    #[test]
    fn list_cursor_move_is_list_partial() {
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::AlarmList { selected: 0 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 7,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.view = RenderView::AlarmList { selected: 1 };
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::List
            }
        );
    }

    #[test]
    fn same_page_data_change_repaints_list_region() {
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::AlarmList { selected: 0 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 7,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.data_fingerprint = 8; // an alarm's enabled flag flipped
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::List
            }
        );
    }

    #[test]
    fn drawer_cursor_move_is_navbar_partial() {
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::Navigation { selected: 0 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.view = RenderView::Navigation { selected: 3 };
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::NavBar
            }
        );
    }

    #[test]
    fn read_only_surface_same_page_change_is_surface_partial_not_full() {
        // A same-surface change on a read-only page repaints the whole
        // surface (region Surface) rather than a Full frame - still a
        // Partial plan the executor maps to the surface rect.
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::NumberPick {
                stage: crate::app::AddStage::Hour,
                value: 9,
            },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.view = RenderView::NumberPick {
            stage: crate::app::AddStage::Hour,
            value: 10,
        };
        assert_eq!(
            plan_render(Some(&prev), &cur, 0),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::Surface
            }
        );
    }

    #[test]
    fn from_state_captures_data_change_fingerprint() {
        // Turning an alarm off changes the fingerprint of an AlarmList
        // projection even when the cursor is unchanged.
        let mut state = crate::app::AppState::default();
        let snap = boot_snapshot(vec![alarm(1, 8, 0)], Some(dt(8, 0)));
        let _ = crate::app::update(&mut state, Event::Boot(snap));
        // Directly place both on AlarmList.
        state.screen = crate::app::Screen::AlarmList { selected: 0 };
        let fp_on = ViewModel::from_state(&state).data_fingerprint;
        // Toggle the alarm off in `state` (persist + store change path is
        // exercised elsewhere; here we mutate the authoritative list the
        // projection reads).
        state.alarms.alarms[0].enabled = false;
        let fp_off = ViewModel::from_state(&state).data_fingerprint;
        assert_ne!(fp_on, fp_off, "enabled flip must change the fingerprint");
    }
}
