use crate::config::types::CurveType;

#[derive(Debug, Clone)]
pub struct CircadianParams {
    pub wake_minutes: u32,
    pub sleep_minutes: u32,
    pub ramp_duration_mins: u32,
    pub start_temp_k: u16,
    pub peak_temp_k: u16,
    pub end_temp_k: u16,
    pub start_brightness: u8,
    pub peak_brightness: u8,
    pub end_brightness: u8,
    pub curve: CurveType,
}

#[derive(Debug, Clone, Copy)]
pub struct CircadianTarget {
    pub brightness: u8,
    pub color_temp_mired: u16,
    pub color_temp_k: u16,
}

fn kelvin_to_mired(k: u16) -> u16 {
    (1_000_000u32 / k as u32) as u16
}

fn cosine_ease(t: f64) -> f64 {
    (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
}

fn interpolate(from: f64, to: f64, t: f64, curve: CurveType) -> f64 {
    let factor = match curve {
        CurveType::Cosine => cosine_ease(t),
        CurveType::Linear => t,
    };
    from + (to - from) * factor
}

pub fn compute_target(params: &CircadianParams, current_minutes: u32) -> CircadianTarget {
    let wake = params.wake_minutes;
    let sleep = params.sleep_minutes;
    let ramp = params.ramp_duration_mins;

    let morning_ramp_end = wake + ramp;
    let evening_ramp_start = sleep.saturating_sub(ramp);

    let (temp_k, brightness) = if current_minutes < wake || current_minutes >= sleep {
        // Night: hold at end values
        (params.end_temp_k, params.end_brightness)
    } else if current_minutes < morning_ramp_end {
        // Morning ramp: start → peak
        let elapsed = current_minutes - wake;
        let t = elapsed as f64 / ramp as f64;
        let temp = interpolate(
            params.start_temp_k as f64,
            params.peak_temp_k as f64,
            t,
            params.curve,
        );
        let bright = interpolate(
            params.start_brightness as f64,
            params.peak_brightness as f64,
            t,
            params.curve,
        );
        (temp as u16, bright as u8)
    } else if current_minutes < evening_ramp_start {
        // Day plateau: hold at peak
        (params.peak_temp_k, params.peak_brightness)
    } else {
        // Evening ramp: peak → end
        let elapsed = current_minutes - evening_ramp_start;
        let t = elapsed as f64 / ramp as f64;
        let temp = interpolate(
            params.peak_temp_k as f64,
            params.end_temp_k as f64,
            t,
            params.curve,
        );
        let bright = interpolate(
            params.peak_brightness as f64,
            params.end_brightness as f64,
            t,
            params.curve,
        );
        (temp as u16, bright as u8)
    };

    CircadianTarget {
        brightness,
        color_temp_mired: kelvin_to_mired(temp_k),
        color_temp_k: temp_k,
    }
}

pub fn parse_time_to_minutes(time_str: &str) -> u32 {
    crate::schedule::TimeOfDay::from_hm_str(time_str)
        .map(|tod| tod.as_minutes() as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params() -> CircadianParams {
        CircadianParams {
            wake_minutes: 360,       // 06:00
            sleep_minutes: 1380,     // 23:00
            ramp_duration_mins: 120, // 2 hours
            start_temp_k: 2700,
            peak_temp_k: 4000,
            end_temp_k: 2200,
            start_brightness: 180,
            peak_brightness: 254,
            end_brightness: 150,
            curve: CurveType::Cosine,
        }
    }

    #[test]
    fn test_night_values() {
        let params = test_params();
        // 03:00 = 180 minutes — before wake
        let target = compute_target(&params, 180);
        assert_eq!(target.brightness, 150);
        assert_eq!(target.color_temp_k, 2200);
    }

    #[test]
    fn test_wake_start() {
        let params = test_params();
        // 06:00 = 360 minutes — start of morning ramp
        let target = compute_target(&params, 360);
        assert_eq!(target.brightness, 180);
        assert_eq!(target.color_temp_k, 2700);
    }

    #[test]
    fn test_midday_plateau() {
        let params = test_params();
        // 12:00 = 720 minutes — well into plateau
        let target = compute_target(&params, 720);
        assert_eq!(target.brightness, 254);
        assert_eq!(target.color_temp_k, 4000);
    }

    #[test]
    fn test_evening_end() {
        let params = test_params();
        // 23:00 = 1380 — at sleep time, should be end values
        let target = compute_target(&params, 1380);
        assert_eq!(target.brightness, 150);
        assert_eq!(target.color_temp_k, 2200);
    }

    #[test]
    fn test_morning_ramp_midpoint_cosine() {
        let params = test_params();
        // 07:00 = 420 minutes — 1 hour into 2-hour ramp = t=0.5
        let target = compute_target(&params, 420);
        // Cosine at t=0.5: (1 - cos(0.5*PI))/2 = 0.5 — exactly halfway
        // Brightness: 180 + (254-180)*0.5 = 217
        assert_eq!(target.brightness, 217);
    }

    #[test]
    fn test_kelvin_to_mired() {
        assert_eq!(kelvin_to_mired(4000), 250);
        assert_eq!(kelvin_to_mired(2700), 370);
        assert_eq!(kelvin_to_mired(6500), 153);
    }

    #[test]
    fn test_parse_time_to_minutes() {
        assert_eq!(parse_time_to_minutes("06:00"), 360);
        assert_eq!(parse_time_to_minutes("23:00"), 1380);
        assert_eq!(parse_time_to_minutes("12:30"), 750);
    }

    #[test]
    fn test_linear_curve() {
        let mut params = test_params();
        params.curve = CurveType::Linear;
        // 07:00 = 420 = 1hr into 2hr ramp = t=0.5
        let target = compute_target(&params, 420);
        // Linear at t=0.5: brightness = 180 + (254-180)*0.5 = 217
        assert_eq!(target.brightness, 217);
    }
}
