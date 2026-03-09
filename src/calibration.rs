use crate::config::types::{AppConfig, LightCalibrationConfig};
use crate::state::{LightType, Z2mDeviceInfo, Z2mGroupInfo};

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedCalibration {
    pub color_temp_offset: i16,
    pub brightness_offset: i16,
}

impl ResolvedCalibration {
    pub const NEUTRAL: Self = Self {
        color_temp_offset: 0,
        brightness_offset: 0,
    };

    pub fn is_neutral(&self) -> bool {
        self.color_temp_offset == 0 && self.brightness_offset == 0
    }

    /// Apply brightness offset, clamping to 1-254.
    pub fn apply_brightness(&self, brightness: u8) -> u8 {
        let adjusted = brightness as i16 + self.brightness_offset;
        adjusted.clamp(1, 254) as u8
    }

    /// Apply color_temp offset, clamping to device min/max mired range.
    pub fn apply_color_temp(&self, mired: u16, device: Option<&Z2mDeviceInfo>) -> u16 {
        let adjusted = mired as i32 + self.color_temp_offset as i32;
        let min = device.and_then(|d| d.color_temp_min).unwrap_or(1) as i32;
        let max = device.and_then(|d| d.color_temp_max).unwrap_or(u16::MAX) as i32;
        adjusted.clamp(min, max) as u16
    }
}

/// Resolve calibration for a specific device.
/// Priority: explicit override > auto-defaults for RGBW > neutral.
pub fn resolve(
    ieee: &str,
    config: &LightCalibrationConfig,
    device_info: Option<&Z2mDeviceInfo>,
) -> ResolvedCalibration {
    // Check explicit override first
    if let Some(ovr) = config.overrides.get(ieee) {
        let light_type = device_info.map(|d| d.light_type());
        let auto = if config.auto_defaults && light_type == Some(LightType::Rgbw) {
            ResolvedCalibration {
                color_temp_offset: config.rgbw_color_temp_offset,
                brightness_offset: config.rgbw_brightness_offset,
            }
        } else {
            ResolvedCalibration::NEUTRAL
        };
        return ResolvedCalibration {
            color_temp_offset: ovr.color_temp_offset.unwrap_or(auto.color_temp_offset),
            brightness_offset: ovr.brightness_offset.unwrap_or(auto.brightness_offset),
        };
    }

    // Auto-defaults for RGBW lights
    if config.auto_defaults
        && let Some(info) = device_info
        && info.light_type() == LightType::Rgbw
    {
        return ResolvedCalibration {
            color_temp_offset: config.rgbw_color_temp_offset,
            brightness_offset: config.rgbw_brightness_offset,
        };
    }

    ResolvedCalibration::NEUTRAL
}

/// Returns true if any member of the group has non-neutral calibration,
/// meaning we need per-device fan-out instead of a single group publish.
pub fn group_needs_fanout(
    group: &Z2mGroupInfo,
    config: &LightCalibrationConfig,
    device_map: &HashMap<String, Z2mDeviceInfo>,
) -> bool {
    group.members.iter().any(|member| {
        let device_info = device_map.get(&member.ieee_address);
        !resolve(&member.ieee_address, config, device_info).is_neutral()
    })
}

/// Convenience: resolve calibration for a device using the full AppConfig and device map.
pub fn resolve_for_device(
    ieee: &str,
    config: &AppConfig,
    device_map: &HashMap<String, Z2mDeviceInfo>,
) -> ResolvedCalibration {
    resolve(ieee, &config.light_calibration, device_map.get(ieee))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::LightCalibrationOverride;

    fn make_device(
        supports_color_temp: bool,
        supports_color_xy: bool,
        min: Option<u16>,
        max: Option<u16>,
    ) -> Z2mDeviceInfo {
        Z2mDeviceInfo {
            ieee_address: "0x0011223344556677".to_string(),
            friendly_name: "test".to_string(),
            supported: true,
            available: true,
            supports_brightness: true,
            supports_color_temp,
            color_temp_min: min,
            color_temp_max: max,
            supports_color_xy,
            supports_color_hs: false,
        }
    }

    fn default_config() -> LightCalibrationConfig {
        LightCalibrationConfig::default()
    }

    #[test]
    fn test_neutral_is_passthrough() {
        let cal = ResolvedCalibration::NEUTRAL;
        assert!(cal.is_neutral());
        assert_eq!(cal.apply_brightness(128), 128);
        assert_eq!(cal.apply_color_temp(300, None), 300);
    }

    #[test]
    fn test_color_temp_offset_positive() {
        let cal = ResolvedCalibration {
            color_temp_offset: 15,
            brightness_offset: 0,
        };
        assert_eq!(cal.apply_color_temp(300, None), 315);
    }

    #[test]
    fn test_color_temp_offset_negative() {
        let cal = ResolvedCalibration {
            color_temp_offset: -20,
            brightness_offset: 0,
        };
        assert_eq!(cal.apply_color_temp(300, None), 280);
    }

    #[test]
    fn test_color_temp_clamped_to_device_range() {
        let device = make_device(true, false, Some(150), Some(500));
        let cal = ResolvedCalibration {
            color_temp_offset: 50,
            brightness_offset: 0,
        };
        // 490 + 50 = 540, clamped to 500
        assert_eq!(cal.apply_color_temp(490, Some(&device)), 500);
    }

    #[test]
    fn test_brightness_offset_with_clamping() {
        let cal = ResolvedCalibration {
            color_temp_offset: 0,
            brightness_offset: -20,
        };
        assert_eq!(cal.apply_brightness(128), 108);
        // Clamp to minimum 1
        assert_eq!(cal.apply_brightness(5), 1);

        let cal_up = ResolvedCalibration {
            color_temp_offset: 0,
            brightness_offset: 50,
        };
        // Clamp to maximum 254
        assert_eq!(cal_up.apply_brightness(230), 254);
    }

    #[test]
    fn test_rgbw_auto_default() {
        let config = default_config(); // auto_defaults=true, rgbw_color_temp_offset=15
        let rgbw = make_device(true, true, None, None);
        let cal = resolve("0x0011223344556677", &config, Some(&rgbw));
        assert_eq!(cal.color_temp_offset, 15);
        assert_eq!(cal.brightness_offset, 0);
        assert!(!cal.is_neutral());
    }

    #[test]
    fn test_ct_only_no_auto_default() {
        let config = default_config();
        let ct = make_device(true, false, None, None);
        let cal = resolve("0x0011223344556677", &config, Some(&ct));
        assert!(cal.is_neutral());
    }

    #[test]
    fn test_explicit_override_beats_auto_default() {
        let mut config = default_config();
        config.overrides.insert(
            "0x0011223344556677".to_string(),
            LightCalibrationOverride {
                color_temp_offset: Some(30),
                brightness_offset: Some(-10),
            },
        );
        let rgbw = make_device(true, true, None, None);
        let cal = resolve("0x0011223344556677", &config, Some(&rgbw));
        assert_eq!(cal.color_temp_offset, 30);
        assert_eq!(cal.brightness_offset, -10);
    }

    #[test]
    fn test_partial_override_inherits_auto_default() {
        let mut config = default_config();
        config.overrides.insert(
            "0x0011223344556677".to_string(),
            LightCalibrationOverride {
                color_temp_offset: Some(30),
                brightness_offset: None, // inherits auto-default (0 for RGBW)
            },
        );
        let rgbw = make_device(true, true, None, None);
        let cal = resolve("0x0011223344556677", &config, Some(&rgbw));
        assert_eq!(cal.color_temp_offset, 30);
        assert_eq!(cal.brightness_offset, 0); // from rgbw_brightness_offset default
    }

    #[test]
    fn test_group_needs_fanout_mixed() {
        use crate::state::{Z2mGroupInfo, Z2mGroupMember};

        let config = default_config();
        let mut device_map = HashMap::new();

        // One RGBW, one CT-only
        let rgbw = make_device(true, true, None, None);
        let ct = Z2mDeviceInfo {
            ieee_address: "0xAABBCCDDEEFF0011".to_string(),
            ..make_device(true, false, None, None)
        };
        device_map.insert("0x0011223344556677".to_string(), rgbw);
        device_map.insert("0xAABBCCDDEEFF0011".to_string(), ct);

        let group = Z2mGroupInfo {
            id: 1,
            friendly_name: "test_group".to_string(),
            members: vec![
                Z2mGroupMember {
                    ieee_address: "0x0011223344556677".to_string(),
                    endpoint: 1,
                },
                Z2mGroupMember {
                    ieee_address: "0xAABBCCDDEEFF0011".to_string(),
                    endpoint: 1,
                },
            ],
            scenes: vec![],
        };

        assert!(group_needs_fanout(&group, &config, &device_map));
    }

    #[test]
    fn test_group_no_fanout_uniform_ct() {
        use crate::state::{Z2mGroupInfo, Z2mGroupMember};

        let config = default_config();
        let mut device_map = HashMap::new();

        // Two CT-only lights — both neutral
        let ct1 = Z2mDeviceInfo {
            ieee_address: "0x0011223344556677".to_string(),
            ..make_device(true, false, None, None)
        };
        let ct2 = Z2mDeviceInfo {
            ieee_address: "0xAABBCCDDEEFF0011".to_string(),
            ..make_device(true, false, None, None)
        };
        device_map.insert("0x0011223344556677".to_string(), ct1);
        device_map.insert("0xAABBCCDDEEFF0011".to_string(), ct2);

        let group = Z2mGroupInfo {
            id: 1,
            friendly_name: "test_group".to_string(),
            members: vec![
                Z2mGroupMember {
                    ieee_address: "0x0011223344556677".to_string(),
                    endpoint: 1,
                },
                Z2mGroupMember {
                    ieee_address: "0xAABBCCDDEEFF0011".to_string(),
                    endpoint: 1,
                },
            ],
            scenes: vec![],
        };

        assert!(!group_needs_fanout(&group, &config, &device_map));
    }
}
