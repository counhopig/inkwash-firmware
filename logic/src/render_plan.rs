use crate::app::{AppState, RenderGeneration, RenderView};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Frame(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PartialRegion {
    Clock,

    NavBar,

    List,

    CalendarGrid,

    Surface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPlan {
    Noop,

    Partial { frame: Frame, region: PartialRegion },

    Full { frame: Frame },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewModel {
    pub generation: RenderGeneration,

    pub view: RenderView,

    pub clock_minute: Option<u32>,

    pub overlay: Overlay,

    pub data_fingerprint: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,

    AlarmRinging,

    Reminder,
}

impl ViewModel {
    pub fn home(generation: RenderGeneration) -> Self {
        ViewModel {
            generation,
            view: RenderView::Home,
            clock_minute: None,
            overlay: Overlay::None,
            data_fingerprint: 0,
        }
    }

    pub fn from_state(state: &AppState) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();

        let data_screen = match &state.screen {
            crate::app::Screen::Navigation { screen_before, .. } => screen_before.as_ref(),
            screen => screen,
        };
        match data_screen {
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
            _ => {}
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

pub fn plan_render(previous: Option<&ViewModel>, current: &ViewModel) -> RenderPlan {
    let frame = Frame(current.generation.0 as u32);

    let Some(prev) = previous else {
        return RenderPlan::Full { frame };
    };

    if current.view == prev.view
        && current.clock_minute == prev.clock_minute
        && current.overlay == prev.overlay
        && current.data_fingerprint == prev.data_fingerprint
    {
        return RenderPlan::Noop;
    }

    if current.overlay != prev.overlay {
        return RenderPlan::Full { frame };
    }

    if same_navigation_background(&prev.view, &current.view)
        && current.clock_minute == prev.clock_minute
        && current.overlay == prev.overlay
        && current.data_fingerprint == prev.data_fingerprint
    {
        return RenderPlan::Partial {
            frame,
            region: PartialRegion::NavBar,
        };
    }

    if is_navigation_view(&prev.view) || is_navigation_view(&current.view) {
        return RenderPlan::Full { frame };
    }

    let same_surface = surface_kind(&current.view) == surface_kind(&prev.view);
    if !same_surface {
        return RenderPlan::Full { frame };
    }

    let region = match surface_kind(&current.view) {
        SurfaceKind::Home if current.clock_minute != prev.clock_minute => PartialRegion::Clock,
        SurfaceKind::Home => return RenderPlan::Full { frame },

        SurfaceKind::NavBar => PartialRegion::NavBar,

        SurfaceKind::List => PartialRegion::List,

        SurfaceKind::CalendarGrid => PartialRegion::CalendarGrid,

        SurfaceKind::ReadOnlySurface => PartialRegion::Surface,
    };

    RenderPlan::Partial { frame, region }
}

fn same_navigation_background(previous: &RenderView, current: &RenderView) -> bool {
    match (previous, current) {
        (
            RenderView::Navigation {
                underlying: previous_source,
                ..
            },
            RenderView::Navigation {
                underlying: current_source,
                ..
            },
        ) => previous_source == current_source,
        (
            RenderView::Navigation {
                underlying: previous_source,
                ..
            },
            current_source,
        ) => previous_source.as_ref() == current_source,
        (
            previous_source,
            RenderView::Navigation {
                underlying: current_source,
                ..
            },
        ) => previous_source == current_source.as_ref(),
        _ => false,
    }
}

fn is_navigation_view(view: &RenderView) -> bool {
    matches!(view, RenderView::Navigation { .. })
}

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
        assert_eq!(plan_render(Some(&vm), &vm), RenderPlan::Noop);
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
        assert_eq!(plan_render(None, &vm), RenderPlan::Full { frame: Frame(0) });
    }

    #[test]
    fn home_minute_tick_is_clock_partial() {
        let prev = home_vm(8 * 60);
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.clock_minute = Some(8 * 60 + 1);
        assert_eq!(
            plan_render(Some(&prev), &cur),
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
            plan_render(Some(&prev), &cur),
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

        let mut ringing = base.clone();
        ringing.generation = crate::app::RenderGeneration(2);
        ringing.overlay = Overlay::AlarmRinging;
        assert_eq!(
            plan_render(Some(&base), &ringing),
            RenderPlan::Full { frame: Frame(2) }
        );

        let mut dismissed = ringing.clone();
        dismissed.generation = crate::app::RenderGeneration(3);
        dismissed.overlay = Overlay::None;
        assert_eq!(
            plan_render(Some(&ringing), &dismissed),
            RenderPlan::Full { frame: Frame(3) }
        );
    }

    #[test]
    fn dismiss_restores_same_view_and_no_partial_after() {
        let mut state = crate::app::AppState::default();
        state.alarms.alarms = vec![alarm(1, 9, 0)];
        let before = ViewModel::from_state(&state);
        assert_eq!(before.overlay, Overlay::None);

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

        let _ = crate::app::update(
            &mut state,
            Event::Button(ButtonEvent::Pressed(ButtonId::Enter)),
        );
        let after = ViewModel::from_state(&state);
        assert_eq!(after.overlay, Overlay::None);

        assert_eq!(after.view, before.view);
        assert_eq!(after.data_fingerprint, before.data_fingerprint);
    }

    #[test]
    fn partial_failure_recovers_full_same_frame() {
        let cur = ViewModel {
            generation: crate::app::RenderGeneration(9),
            view: RenderView::Settings { selected: 1 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        assert_eq!(
            plan_render(None, &cur),
            RenderPlan::Full { frame: Frame(9) }
        );
    }

    #[test]
    fn home_minute_changes_stay_partial_without_maintenance_full() {
        let mut previous = home_vm(8 * 60);
        for generation in 1_u64..=256 {
            let current = ViewModel {
                generation: crate::app::RenderGeneration(generation),
                clock_minute: Some(8 * 60 + generation as u32),
                ..previous.clone()
            };
            assert!(matches!(
                plan_render(Some(&previous), &current),
                RenderPlan::Partial {
                    region: PartialRegion::Clock,
                    ..
                }
            ));
            previous = current;
        }
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
            plan_render(Some(&prev), &cur),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::List
            }
        );
    }

    #[test]
    fn list_window_scroll_to_last_row_stays_on_list_partial() {
        let prev = ViewModel {
            generation: crate::app::RenderGeneration(1),
            view: RenderView::AlarmList { selected: 6 },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 7,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.view = RenderView::AlarmList { selected: 7 };
        assert_eq!(
            plan_render(Some(&prev), &cur),
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
        cur.data_fingerprint = 8;
        assert_eq!(
            plan_render(Some(&prev), &cur),
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
            view: RenderView::Navigation {
                selected: 0,
                underlying: Box::new(RenderView::Home),
            },
            clock_minute: Some(8 * 60),
            overlay: Overlay::None,
            data_fingerprint: 0,
        };
        let mut cur = prev.clone();
        cur.generation = crate::app::RenderGeneration(2);
        cur.view = RenderView::Navigation {
            selected: 3,
            underlying: Box::new(RenderView::Home),
        };
        assert_eq!(
            plan_render(Some(&prev), &cur),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::NavBar
            }
        );
    }

    #[test]
    fn drawer_open_move_close_is_navbar_partial_for_home_calendar_and_inbox() {
        let sources = [
            RenderView::Home,
            RenderView::Calendar {
                year: 2026,
                month: 9,
                selected_day: 11,
            },
            RenderView::Inbox { selected: 2 },
        ];
        for source in sources {
            let source_vm = ViewModel {
                generation: RenderGeneration(1),
                view: source.clone(),
                clock_minute: Some(12 * 60),
                overlay: Overlay::None,
                data_fingerprint: 17,
            };
            let open_vm = ViewModel {
                generation: RenderGeneration(2),
                view: RenderView::Navigation {
                    selected: 1,
                    underlying: Box::new(source.clone()),
                },
                ..source_vm.clone()
            };
            assert_eq!(
                plan_render(Some(&source_vm), &open_vm),
                RenderPlan::Partial {
                    frame: Frame(2),
                    region: PartialRegion::NavBar,
                }
            );

            let moved_vm = ViewModel {
                generation: RenderGeneration(3),
                view: RenderView::Navigation {
                    selected: 4,
                    underlying: Box::new(source.clone()),
                },
                ..open_vm.clone()
            };
            assert_eq!(
                plan_render(Some(&open_vm), &moved_vm),
                RenderPlan::Partial {
                    frame: Frame(3),
                    region: PartialRegion::NavBar,
                }
            );

            let close_vm = ViewModel {
                generation: RenderGeneration(4),
                view: source.clone(),
                ..moved_vm.clone()
            };
            assert_eq!(
                plan_render(Some(&moved_vm), &close_vm),
                RenderPlan::Partial {
                    frame: Frame(4),
                    region: PartialRegion::NavBar,
                }
            );
        }
    }

    #[test]
    fn drawer_transition_is_full_when_destination_or_source_facts_change() {
        let source = RenderView::Inbox { selected: 0 };
        let prev = ViewModel {
            generation: RenderGeneration(1),
            view: RenderView::Navigation {
                selected: 2,
                underlying: Box::new(source.clone()),
            },
            clock_minute: Some(12 * 60),
            overlay: Overlay::None,
            data_fingerprint: 5,
        };

        let destination = ViewModel {
            generation: RenderGeneration(2),
            view: RenderView::Calendar {
                year: 2026,
                month: 9,
                selected_day: 4,
            },
            ..prev.clone()
        };
        assert!(matches!(
            plan_render(Some(&prev), &destination),
            RenderPlan::Full { .. }
        ));

        let mut data_changed = prev.clone();
        data_changed.generation = RenderGeneration(3);
        data_changed.data_fingerprint += 1;
        assert!(matches!(
            plan_render(Some(&prev), &data_changed),
            RenderPlan::Full { .. }
        ));

        let mut clock_changed = prev.clone();
        clock_changed.generation = RenderGeneration(4);
        clock_changed.clock_minute = Some(12 * 60 + 1);
        assert!(matches!(
            plan_render(Some(&prev), &clock_changed),
            RenderPlan::Full { .. }
        ));

        let mut different_source = prev.clone();
        different_source.generation = RenderGeneration(5);
        different_source.view = RenderView::Navigation {
            selected: 2,
            underlying: Box::new(RenderView::Inbox { selected: 1 }),
        };
        assert!(matches!(
            plan_render(Some(&prev), &different_source),
            RenderPlan::Full { .. }
        ));
    }

    #[test]
    fn read_only_surface_same_page_change_is_surface_partial_not_full() {
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
            plan_render(Some(&prev), &cur),
            RenderPlan::Partial {
                frame: Frame(2),
                region: PartialRegion::Surface
            }
        );
    }

    #[test]
    fn from_state_captures_data_change_fingerprint() {
        let mut state = crate::app::AppState::default();
        let snap = boot_snapshot(vec![alarm(1, 8, 0)], Some(dt(8, 0)));
        let _ = crate::app::update(&mut state, Event::Boot(snap));

        state.screen = crate::app::Screen::AlarmList { selected: 0 };
        let fp_on = ViewModel::from_state(&state).data_fingerprint;

        state.alarms.alarms[0].enabled = false;
        let fp_off = ViewModel::from_state(&state).data_fingerprint;
        assert_ne!(fp_on, fp_off, "enabled flip must change the fingerprint");
    }
}
