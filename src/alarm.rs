//! Wake-up alarms: fade a room's lights in slowly so they are fully on at the
//! alarm time, like a sunrise.
//!
//! jeha drives the fade itself, one small step every `STEP_SECS` with a Z2M
//! transition of the same length, instead of sending one long transition.
//! Bulbs handle long transitions inconsistently, and stepping lets the fade
//! stop the moment someone touches the lights.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::circadian::CircadianEngine;
use crate::config::types::{AlarmConfig, AppConfig, RoomConfig};
use crate::event::{Event, EventBus};
use crate::mqtt::publish::Publisher;
use crate::schedule::DayOfWeek;
use crate::state::{RoomState, RoomStateUpdate, SharedState, StateCommand, UpdateSource};

/// Seconds between fade steps. Each step is published with a transition of
/// the same length, so the bulbs move continuously.
const STEP_SECS: i64 = 15;

/// Where every fade starts: the lowest brightness, warmest white.
const START_BRIGHTNESS: u8 = 1;
const START_COLOR_TEMP_K: u16 = 2000;

/// Perceived-brightness exponent. The fade is linear in perceived brightness,
/// so the first minutes don't look like a jump from dark to dim.
const PERCEPTUAL_GAMMA: f64 = 2.2;

fn kelvin_to_mired(k: u16) -> u16 {
    (1_000_000u32 / k as u32) as u16
}

/// The light values a fade ends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadeTarget {
    pub brightness: u8,
    pub color_temp_mired: u16,
}

/// Brightness and colour temperature `progress` (0.0-1.0) of the way through a fade.
pub fn fade_point(progress: f64, target: FadeTarget) -> (u8, u16) {
    let t = progress.clamp(0.0, 1.0);
    let perceived = |b: u8| (b as f64 / 254.0).powf(1.0 / PERCEPTUAL_GAMMA);
    let p0 = perceived(START_BRIGHTNESS);
    let p1 = perceived(target.brightness);
    let brightness = ((p0 + (p1 - p0) * t).powf(PERCEPTUAL_GAMMA) * 254.0)
        .round()
        .clamp(1.0, 254.0) as u8;
    let m0 = kelvin_to_mired(START_COLOR_TEMP_K) as f64;
    let m1 = target.color_temp_mired as f64;
    let mired = (m0 + (m1 - m0) * t).round() as u16;
    (brightness, mired)
}

/// The moment `alarm` is fully on for `date`, in local time. A time skipped by
/// a DST change rings an hour later instead of not at all.
fn ring_time(alarm: &AlarmConfig, date: NaiveDate, tz: Tz) -> Option<DateTime<Tz>> {
    let minutes = alarm.time.as_minutes() as u32;
    let naive = date.and_hms_opt(minutes / 60, minutes % 60, 0)?;
    tz.from_local_datetime(&naive).earliest().or_else(|| {
        tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
    })
}

fn rings_on(alarm: &AlarmConfig, date: NaiveDate) -> bool {
    alarm.days.is_empty() || alarm.days.contains(&DayOfWeek::from_chrono(date.weekday()))
}

/// A fade window: the lights start fading in at `start` and are fully on at `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The day the alarm rings on (the date of `end`).
    pub date: NaiveDate,
    pub start: DateTime<Tz>,
    pub end: DateTime<Tz>,
}

/// The fade window `now` falls in, if any. Looks at tomorrow too, so a fade
/// for an alarm just after midnight starts the evening before.
pub fn due(alarm: &AlarmConfig, now: DateTime<Tz>) -> Option<Window> {
    if !alarm.enabled {
        return None;
    }
    let today = now.date_naive();
    [today, today.succ_opt()?].into_iter().find_map(|date| {
        if !rings_on(alarm, date) {
            return None;
        }
        let end = ring_time(alarm, date, now.timezone())?;
        let start = end - chrono::Duration::minutes(alarm.fade_mins as i64);
        (start <= now && now < end).then_some(Window { date, start, end })
    })
}

/// When the alarm is next fully on, or `None` if it's disabled or never rings.
pub fn next_ring(alarm: &AlarmConfig, now: DateTime<Tz>) -> Option<DateTime<Tz>> {
    if !alarm.enabled {
        return None;
    }
    let today = now.date_naive();
    (0..8)
        .filter_map(|offset| today.checked_add_days(chrono::Days::new(offset)))
        .filter(|date| rings_on(alarm, *date))
        .filter_map(|date| ring_time(alarm, date, now.timezone()))
        .find(|end| *end > now)
}

/// Why a running fade should stop, if someone has taken over the room.
fn interruption(room: Option<&RoomState>) -> Option<&'static str> {
    let Some(room) = room else {
        return Some("room state is gone");
    };
    if !room.lights_on {
        Some("lights were turned off")
    } else if room.night_mode_active {
        Some("night mode was switched on")
    } else if room.update_source != UpdateSource::Automation {
        Some("lights were changed")
    } else if !room.circadian_paused {
        Some("circadian was resumed")
    } else {
        None
    }
}

/// Starts alarm fades when their window opens.
pub struct AlarmScheduler {
    config: Arc<AppConfig>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    circadian_engine: Option<Arc<CircadianEngine>>,
    cancel: CancellationToken,
    /// (alarm id, date) pairs that already rang, so each alarm rings once a day
    /// even if the fade is stopped early.
    rung: HashSet<(String, NaiveDate)>,
    /// Fades in progress, by room.
    running: HashMap<String, JoinHandle<()>>,
}

impl AlarmScheduler {
    pub fn new(
        config: Arc<AppConfig>,
        state: SharedState,
        state_tx: mpsc::Sender<StateCommand>,
        publisher: Arc<Publisher>,
        event_bus: EventBus,
        cancel: CancellationToken,
        circadian_engine: Option<Arc<CircadianEngine>>,
    ) -> Self {
        Self {
            config,
            state,
            state_tx,
            publisher,
            event_bus,
            circadian_engine,
            cancel,
            rung: HashSet::new(),
            running: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        if self.config.alarms.is_empty() {
            debug!("No alarms configured");
            return;
        }
        info!(
            "Alarm scheduler started ({} alarms)",
            self.config.alarms.len()
        );
        let mut interval = crate::schedule::aligned_interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("Alarm scheduler shutting down");
                    return;
                }
                _ = interval.tick() => self.check(),
            }
        }
    }

    fn timezone(&self) -> Tz {
        self.config
            .general
            .timezone
            .parse()
            .unwrap_or(chrono_tz::UTC)
    }

    fn check(&mut self) {
        let current = self.state.load();
        // Publishes would queue up and replay late; try again on the next tick.
        if !current.mqtt_connected {
            return;
        }
        let now = Utc::now().with_timezone(&self.timezone());
        let yesterday = now.date_naive().pred_opt().unwrap_or(now.date_naive());
        self.rung.retain(|(_, date)| *date >= yesterday);
        self.running.retain(|_, handle| !handle.is_finished());

        for alarm in current.effective_alarms(&self.config.alarms) {
            let Some(window) = due(&alarm, now) else {
                continue;
            };
            if !self.rung.insert((alarm.id.clone(), window.date)) {
                continue;
            }
            for room_id in &alarm.rooms {
                let Some(room_config) = self.config.rooms.get(room_id) else {
                    continue;
                };
                if self.running.contains_key(room_id) {
                    info!(
                        "Alarm '{}': skipping '{}', another alarm is already fading it in",
                        alarm.id, room_id
                    );
                    continue;
                }
                let lights_on = current.rooms.get(room_id).is_some_and(|r| r.lights_on);
                if lights_on {
                    info!(
                        "Alarm '{}': skipping '{}', the lights are already on",
                        alarm.id, room_id
                    );
                    continue;
                }
                let (target, handover) = self.fade_target(&alarm, room_id, window.end);
                let fade = Fade {
                    alarm_id: alarm.id.clone(),
                    room_id: room_id.clone(),
                    room_config: room_config.clone(),
                    window,
                    target,
                    handover,
                    state: self.state.clone(),
                    state_tx: self.state_tx.clone(),
                    publisher: self.publisher.clone(),
                    event_bus: self.event_bus.clone(),
                    cancel: self.cancel.clone(),
                };
                self.running
                    .insert(room_id.clone(), tokio::spawn(fade.run()));
            }
        }
    }

    /// The values the fade ends on, and when circadian takes the room back.
    ///
    /// By default the fade ends where circadian will be at the alarm time, so
    /// the hand-over is seamless. An alarm before wake_time ends on the wake-up
    /// values instead (circadian would otherwise hold the dim night values),
    /// and the lights stay there until wake_time.
    fn fade_target(
        &self,
        alarm: &AlarmConfig,
        room_id: &str,
        end: DateTime<Tz>,
    ) -> (FadeTarget, DateTime<Tz>) {
        let end_minutes = end.hour() * 60 + end.minute();
        let wake_minutes = self
            .circadian_engine
            .as_ref()
            .and_then(|engine| engine.room_wake_minutes(room_id));
        let eval_minutes = wake_minutes.map_or(end_minutes, |wake| end_minutes.max(wake));
        let circadian = self
            .circadian_engine
            .as_ref()
            .and_then(|engine| engine.compute_room_target_at(room_id, eval_minutes));

        let target = FadeTarget {
            brightness: alarm
                .brightness
                .or(circadian.map(|c| c.brightness))
                .unwrap_or(254),
            color_temp_mired: alarm
                .color_temp_k
                .map(kelvin_to_mired)
                .or(circadian.map(|c| c.color_temp_mired))
                .unwrap_or_else(|| kelvin_to_mired(self.config.circadian.defaults.start_temp_k)),
        };

        let handover = match wake_minutes {
            Some(wake) if wake > end_minutes => end
                .date_naive()
                .and_hms_opt(wake / 60, wake % 60, 0)
                .and_then(|naive| end.timezone().from_local_datetime(&naive).earliest())
                .unwrap_or(end),
            _ => end,
        };
        (target, handover)
    }
}

/// One alarm fading in one room.
struct Fade {
    alarm_id: String,
    room_id: String,
    room_config: RoomConfig,
    window: Window,
    target: FadeTarget,
    handover: DateTime<Tz>,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    event_bus: EventBus,
    cancel: CancellationToken,
}

impl Fade {
    async fn update(&self, update: RoomStateUpdate) {
        let _ = self
            .state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: self.room_id.clone(),
                update,
            })
            .await;
    }

    async fn publish(&self, brightness: u8, mired: u16, transition: u32) -> anyhow::Result<()> {
        if let Some(ref group) = self.room_config.z2m_group {
            self.publisher
                .turn_on_group(group, Some(brightness), Some(mired), Some(transition))
                .await
        } else {
            for ieee in &self.room_config.lights {
                self.publisher
                    .turn_on_ieee(ieee, Some(brightness), Some(mired), Some(transition))
                    .await?;
            }
            Ok(())
        }
    }

    async fn run(self) {
        let tz = self.window.end.timezone();
        let now = Utc::now().with_timezone(&tz);
        let hold = (self.handover - now).to_std().unwrap_or_default();
        let pause_until = Instant::now() + hold;

        info!(
            "Alarm '{}': fading in '{}' until {} (brightness {}, {}K)",
            self.alarm_id,
            self.room_id,
            self.window.end.format("%H:%M"),
            self.target.brightness,
            1_000_000u32 / self.target.color_temp_mired.max(1) as u32,
        );

        // Waking up ends the night. The event also stops the night mode
        // schedule from switching it straight back on.
        let night_mode = self
            .state
            .load()
            .rooms
            .get(&self.room_id)
            .is_some_and(|r| r.night_mode_active);
        if night_mode {
            self.update(RoomStateUpdate::NightMode(false)).await;
            self.event_bus.publish(Event::NightModeChanged {
                room_id: self.room_id.clone(),
                active: false,
            });
        }
        // Keep circadian off the lights during the fade, and until wake_time
        // for an early alarm. It takes over by itself when the pause runs out.
        self.update(RoomStateUpdate::CircadianPause {
            paused: true,
            until: Some(pause_until),
        })
        .await;
        self.update(RoomStateUpdate::Alarm(true)).await;

        let fade_secs = (self.window.end - self.window.start)
            .num_milliseconds()
            .max(1) as f64;
        let mut first = true;
        loop {
            if !first {
                let current = self.state.load();
                let room = current.rooms.get(&self.room_id);
                if let Some(reason) = interruption(room) {
                    info!(
                        "Alarm '{}': stopped fading in '{}', {}",
                        self.alarm_id, self.room_id, reason
                    );
                    // Leave a pause someone else set alone; only lift our own.
                    if room.is_some_and(|r| {
                        r.circadian_paused && r.circadian_paused_until == Some(pause_until)
                    }) {
                        self.update(RoomStateUpdate::CircadianPause {
                            paused: false,
                            until: None,
                        })
                        .await;
                    }
                    break;
                }
            }
            first = false;

            let now = Utc::now().with_timezone(&tz);
            let next = (now + chrono::Duration::seconds(STEP_SECS)).min(self.window.end);
            let progress = (next - self.window.start).num_milliseconds() as f64 / fade_secs;
            let (brightness, mired) = fade_point(progress, self.target);
            let step = (next - now).to_std().unwrap_or_default();
            let transition = step.as_secs_f64().ceil().max(1.0) as u32;

            self.update(RoomStateUpdate::LightsOnWithPush {
                brightness: Some(brightness),
                color_temp_mired: Some(mired),
                source: UpdateSource::Automation,
            })
            .await;
            if let Err(e) = self.publish(brightness, mired, transition).await {
                warn!(
                    "Alarm '{}': fade step failed in '{}': {}",
                    self.alarm_id, self.room_id, e
                );
            }

            if next >= self.window.end {
                info!("Alarm '{}': '{}' is fully on", self.alarm_id, self.room_id);
                break;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(step) => {}
            }
        }

        self.update(RoomStateUpdate::Alarm(false)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TimeOfDay;

    fn alarm(time: &str, days: Vec<DayOfWeek>) -> AlarmConfig {
        AlarmConfig {
            id: "wake".to_string(),
            rooms: vec!["bedroom".to_string()],
            time: TimeOfDay::from_hm_str(time).unwrap(),
            days,
            fade_mins: 30,
            brightness: None,
            color_temp_k: None,
            enabled: true,
        }
    }

    fn at(tz: Tz, y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        tz.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    const TZ: Tz = chrono_tz::Europe::Stockholm;

    #[test]
    fn test_due_inside_fade_window() {
        // Saturday 2026-10-17
        let a = alarm("06:30", vec![]);
        assert!(due(&a, at(TZ, 2026, 10, 17, 5, 59)).is_none());
        let w = due(&a, at(TZ, 2026, 10, 17, 6, 0)).unwrap();
        assert_eq!(w.start, at(TZ, 2026, 10, 17, 6, 0));
        assert_eq!(w.end, at(TZ, 2026, 10, 17, 6, 30));
        assert!(due(&a, at(TZ, 2026, 10, 17, 6, 29)).is_some());
        // Fully on at 06:30: the window is over.
        assert!(due(&a, at(TZ, 2026, 10, 17, 6, 30)).is_none());
    }

    #[test]
    fn test_due_respects_days() {
        let weekdays = vec![
            DayOfWeek::Mon,
            DayOfWeek::Tue,
            DayOfWeek::Wed,
            DayOfWeek::Thu,
            DayOfWeek::Fri,
        ];
        let a = alarm("06:30", weekdays);
        // Saturday: no alarm
        assert!(due(&a, at(TZ, 2026, 10, 17, 6, 15)).is_none());
        // Monday: alarm
        assert!(due(&a, at(TZ, 2026, 10, 19, 6, 15)).is_some());
    }

    #[test]
    fn test_due_disabled() {
        let mut a = alarm("06:30", vec![]);
        a.enabled = false;
        assert!(due(&a, at(TZ, 2026, 10, 17, 6, 15)).is_none());
        assert!(next_ring(&a, at(TZ, 2026, 10, 17, 6, 15)).is_none());
    }

    #[test]
    fn test_fade_crossing_midnight_uses_day_of_alarm() {
        // 00:10 on Mondays only: the fade starts Sunday 23:40.
        let a = alarm("00:10", vec![DayOfWeek::Mon]);
        let w = due(&a, at(TZ, 2026, 10, 18, 23, 45)).unwrap();
        assert_eq!(w.date, NaiveDate::from_ymd_opt(2026, 10, 19).unwrap());
        assert_eq!(w.end, at(TZ, 2026, 10, 19, 0, 10));
        // Saturday 23:45 is not followed by a Monday.
        assert!(due(&a, at(TZ, 2026, 10, 17, 23, 45)).is_none());
    }

    #[test]
    fn test_due_on_dst_gap() {
        // Stockholm skips 02:00-03:00 on 2026-03-29: 02:30 rings at 03:30.
        let a = alarm("02:30", vec![]);
        let w = due(&a, at(TZ, 2026, 3, 29, 3, 15)).unwrap();
        assert_eq!(w.end, at(TZ, 2026, 3, 29, 3, 30));
    }

    #[test]
    fn test_next_ring() {
        let a = alarm("06:30", vec![DayOfWeek::Mon]);
        // Saturday evening -> Monday morning
        assert_eq!(
            next_ring(&a, at(TZ, 2026, 10, 17, 20, 0)),
            Some(at(TZ, 2026, 10, 19, 6, 30))
        );
        // Monday after the alarm -> next Monday
        assert_eq!(
            next_ring(&a, at(TZ, 2026, 10, 19, 7, 0)),
            Some(at(TZ, 2026, 10, 26, 6, 30))
        );
    }

    #[test]
    fn test_fade_point_endpoints_and_monotonic() {
        let target = FadeTarget {
            brightness: 200,
            color_temp_mired: 333,
        };
        assert_eq!(fade_point(0.0, target), (1, 500));
        assert_eq!(fade_point(1.0, target), (200, 333));
        assert_eq!(fade_point(1.5, target), (200, 333));

        let mut last = (0u8, u16::MAX);
        for i in 0..=100 {
            let (b, m) = fade_point(i as f64 / 100.0, target);
            assert!(b >= last.0, "brightness must not drop");
            assert!(m <= last.1, "colour must only get cooler");
            last = (b, m);
        }
    }

    #[test]
    fn test_fade_starts_gently() {
        // A third of the way in, the room is still well under half brightness.
        let target = FadeTarget {
            brightness: 254,
            color_temp_mired: 250,
        };
        let (b, _) = fade_point(1.0 / 3.0, target);
        assert!(b < 70, "got {b}");
    }

    #[test]
    fn test_interruption() {
        let fading = RoomState {
            lights_on: true,
            update_source: UpdateSource::Automation,
            circadian_paused: true,
            ..Default::default()
        };
        assert_eq!(interruption(Some(&fading)), None);

        let off = RoomState {
            lights_on: false,
            ..fading.clone()
        };
        assert!(interruption(Some(&off)).is_some());

        let dimmed = RoomState {
            update_source: UpdateSource::Manual,
            ..fading.clone()
        };
        assert!(interruption(Some(&dimmed)).is_some());

        let night = RoomState {
            night_mode_active: true,
            ..fading.clone()
        };
        assert!(interruption(Some(&night)).is_some());

        let resumed = RoomState {
            circadian_paused: false,
            ..fading
        };
        assert!(interruption(Some(&resumed)).is_some());
    }

    /// A fade whose window is nearly over: one step to the end, then the room
    /// is left on at the target with circadian held off until hand-over.
    #[tokio::test]
    async fn test_fade_runs_to_target() {
        let config: AppConfig =
            toml::from_str("schema_version = 1\n[rooms.bedroom]\nz2m_group = \"Bedroom\"").unwrap();
        let config = Arc::new(config);
        let state = crate::state::new_shared_state();
        let event_bus = EventBus::new(16);
        let mut events = event_bus.subscribe();
        let (manager, state_tx) = crate::state::StateManager::new(state.clone(), event_bus.clone());
        tokio::spawn(manager.run());
        state_tx
            .send(StateCommand::UpdateRoomState {
                room_id: "bedroom".to_string(),
                update: RoomStateUpdate::NightMode(true),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Keep the event loop alive so publishes have somewhere to queue.
        let (client, _eventloop) =
            rumqttc::AsyncClient::new(rumqttc::MqttOptions::new("test", "localhost", 1883), 64);
        let publisher = Arc::new(Publisher::new(
            client,
            "zigbee2mqtt".to_string(),
            state.clone(),
            config.clone(),
        ));

        let now = Utc::now().with_timezone(&chrono_tz::UTC);
        let end = now + chrono::Duration::milliseconds(1500);
        let target = FadeTarget {
            brightness: 180,
            color_temp_mired: 370,
        };
        let fade = Fade {
            alarm_id: "wake".to_string(),
            room_id: "bedroom".to_string(),
            room_config: config.rooms["bedroom"].clone(),
            window: Window {
                date: end.date_naive(),
                start: end - chrono::Duration::minutes(30),
                end,
            },
            target,
            handover: end + chrono::Duration::minutes(10),
            state: state.clone(),
            state_tx: state_tx.clone(),
            publisher,
            event_bus: event_bus.clone(),
            cancel: CancellationToken::new(),
        };
        tokio::time::timeout(Duration::from_secs(5), fade.run())
            .await
            .expect("fade should finish at the end of its window");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let current = state.load();
        let room = &current.rooms["bedroom"];
        assert!(room.lights_on);
        assert_eq!(room.intended_brightness, Some(180));
        assert_eq!(room.intended_color_temp_mired, Some(370));
        assert!(!room.night_mode_active, "waking up ends night mode");
        assert!(!room.alarm_active);
        assert!(
            room.is_circadian_paused(),
            "circadian waits for the hand-over"
        );
        assert!(matches!(
            events.try_recv(),
            Ok(Event::NightModeChanged { active: false, .. })
        ));
    }
}
