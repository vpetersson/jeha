use crate::config::types::TriggerConfig;
use crate::event::Event;

pub fn matches_trigger(trigger: &TriggerConfig, event: &Event, sensor_ieee: Option<&str>) -> bool {
    match (trigger, event) {
        (
            TriggerConfig::Motion,
            Event::MotionDetected {
                sensor_ieee: event_ieee,
                ..
            },
        ) => sensor_ieee.is_some_and(|s| s == event_ieee),

        (
            TriggerConfig::MotionCleared,
            Event::MotionCleared {
                sensor_ieee: event_ieee,
                ..
            },
        ) => sensor_ieee.is_some_and(|s| s == event_ieee),

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_motion_trigger_match() {
        let trigger = TriggerConfig::Motion;
        let event = Event::MotionDetected {
            room_id: "kitchen".to_string(),
            sensor_ieee: "0x00158d000AAAAAAA".to_string(),
        };
        assert!(matches_trigger(
            &trigger,
            &event,
            Some("0x00158d000AAAAAAA")
        ));
        assert!(!matches_trigger(
            &trigger,
            &event,
            Some("0x00158d000BBBBBBB")
        ));
    }

    #[test]
    fn test_motion_cleared_trigger_match() {
        let trigger = TriggerConfig::MotionCleared;
        let event = Event::MotionCleared {
            room_id: "kitchen".to_string(),
            sensor_ieee: "0x00158d000AAAAAAA".to_string(),
        };
        assert!(matches_trigger(
            &trigger,
            &event,
            Some("0x00158d000AAAAAAA")
        ));
    }

    #[test]
    fn test_no_sensor_no_match() {
        let trigger = TriggerConfig::Motion;
        let event = Event::MotionDetected {
            room_id: "kitchen".to_string(),
            sensor_ieee: "0x00158d000AAAAAAA".to_string(),
        };
        assert!(!matches_trigger(&trigger, &event, None));
    }
}
