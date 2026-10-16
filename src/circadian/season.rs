//! Winter mode: lift circadian brightness when the days are short.
//!
//! Daylight hours at the configured latitude decide how far brightness moves from
//! the regular curve towards the winter values. Absolute day length is used rather
//! than "how far through the year we are", so a home near the equator (≈12h all
//! year) gets a constant mild lift while Stockholm swings fully between the two.

/// Daylight at or above this gets the regular circadian brightness.
const LONG_DAY_HOURS: f64 = 16.0;
/// Daylight at or below this gets the full winter brightness.
const SHORT_DAY_HOURS: f64 = 8.0;

/// Sun altitude at sunrise/sunset: refraction plus the solar disc radius.
const SUNRISE_ALTITUDE_DEG: f64 = -0.833;

/// Hours between sunrise and sunset at `latitude` on `day_of_year` (1-366).
/// Returns 0.0 during polar night and 24.0 during midnight sun.
pub fn day_length_hours(latitude: f64, day_of_year: u16) -> f64 {
    // Solar declination (Cooper's approximation), accurate to well under a degree.
    let declination = (-23.44_f64).to_radians()
        * (2.0 * std::f64::consts::PI * (day_of_year as f64 + 10.0) / 365.0).cos();
    let lat = latitude.to_radians();
    let cos_hour_angle = (SUNRISE_ALTITUDE_DEG.to_radians().sin() - lat.sin() * declination.sin())
        / (lat.cos() * declination.cos());
    if cos_hour_angle <= -1.0 {
        return 24.0;
    }
    if cos_hour_angle >= 1.0 {
        return 0.0;
    }
    2.0 * cos_hour_angle.acos().to_degrees() / 15.0
}

/// How much of the winter brightness to apply: 0.0 on long days, 1.0 on short days.
pub fn winter_weight(latitude: f64, day_of_year: u16) -> f64 {
    let hours = day_length_hours(latitude, day_of_year);
    ((LONG_DAY_HOURS - hours) / (LONG_DAY_HOURS - SHORT_DAY_HOURS)).clamp(0.0, 1.0)
}

/// Blend a regular brightness with its winter counterpart.
pub fn blend_brightness(regular: u8, winter: u8, weight: f64) -> u8 {
    (regular as f64 + (winter as f64 - regular as f64) * weight).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOCKHOLM: f64 = 59.33;
    const SYDNEY: f64 = -33.87;
    const TROMSO: f64 = 69.65;
    // Day-of-year for the solstices (non-leap year).
    const JUNE_21: u16 = 172;
    const DEC_21: u16 = 355;

    #[test]
    fn test_stockholm_day_length() {
        // Published: ~18h37m in June, ~6h05m in December.
        let summer = day_length_hours(STOCKHOLM, JUNE_21);
        let winter = day_length_hours(STOCKHOLM, DEC_21);
        assert!((summer - 18.6).abs() < 0.3, "summer {summer}");
        assert!((winter - 6.1).abs() < 0.3, "winter {winter}");
    }

    #[test]
    fn test_equator_is_about_twelve_hours() {
        for doy in [1, 80, JUNE_21, 266, DEC_21] {
            let h = day_length_hours(0.0, doy);
            assert!((h - 12.1).abs() < 0.2, "day {doy}: {h}");
        }
    }

    #[test]
    fn test_polar_night_and_midnight_sun() {
        assert_eq!(day_length_hours(TROMSO, DEC_21), 0.0);
        assert_eq!(day_length_hours(TROMSO, JUNE_21), 24.0);
    }

    #[test]
    fn test_weight_follows_seasons_in_stockholm() {
        assert_eq!(winter_weight(STOCKHOLM, JUNE_21), 0.0);
        assert_eq!(winter_weight(STOCKHOLM, DEC_21), 1.0);
        // Equinox: ~12h → halfway
        let equinox = winter_weight(STOCKHOLM, 80);
        assert!((equinox - 0.5).abs() < 0.05, "equinox {equinox}");
    }

    #[test]
    fn test_southern_hemisphere_is_inverted() {
        // Sydney: June is winter, December is summer.
        assert!(winter_weight(SYDNEY, JUNE_21) > winter_weight(SYDNEY, DEC_21));
        // ~10h in June, ~14.4h in December
        assert!((day_length_hours(SYDNEY, JUNE_21) - 9.9).abs() < 0.3);
        assert!((day_length_hours(SYDNEY, DEC_21) - 14.4).abs() < 0.3);
    }

    #[test]
    fn test_blend_brightness() {
        assert_eq!(blend_brightness(150, 230, 0.0), 150);
        assert_eq!(blend_brightness(150, 230, 1.0), 230);
        assert_eq!(blend_brightness(150, 230, 0.5), 190);
        // Winter value may also be lower than the regular one
        assert_eq!(blend_brightness(200, 100, 0.5), 150);
    }
}
