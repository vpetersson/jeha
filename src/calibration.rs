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

/// Values for one group member that the group command can't carry.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberPush {
    pub ieee: String,
    pub brightness: Option<u8>,
    pub color_temp_mired: Option<u16>,
}

/// How to publish brightness/color_temp to a Z2M group so that every member
/// ends up at its own calibrated, range-clamped value.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupPublishPlan {
    /// Brightness for the group command, or None when members disagree.
    pub group_brightness: Option<u8>,
    /// Color temp for the group command, or None when members disagree.
    pub group_color_temp_mired: Option<u16>,
    /// Per-device values for the fields left out of the group command.
    pub members: Vec<MemberPush>,
}

/// Plan a group publish. A field goes into the group command only when every
/// member that supports it resolves to the same value; otherwise it's left
/// out of the group command and sent to each member individually.
///
/// A group command reaches every member, so a member with a different value
/// must never receive the field from the group: sending the group value and
/// correcting it per device afterwards makes that bulb swing between the two
/// on every publish.
pub fn plan_group_publish(
    group: &Z2mGroupInfo,
    config: &LightCalibrationConfig,
    device_map: &HashMap<String, Z2mDeviceInfo>,
    brightness: Option<u8>,
    color_temp_mired: Option<u16>,
) -> GroupPublishPlan {
    let mut brightness_values: Vec<(&str, u8)> = Vec::new();
    let mut ct_values: Vec<(&str, u16)> = Vec::new();

    for member in &group.members {
        let Some(device) = device_map.get(&member.ieee_address) else {
            continue;
        };
        let cal = resolve(&member.ieee_address, config, Some(device));
        if let Some(b) = brightness
            && device.supports_brightness
        {
            brightness_values.push((&member.ieee_address, cal.apply_brightness(b)));
        }
        if let Some(ct) = color_temp_mired
            && device.supports_color_temp
        {
            let min = device.color_temp_min.unwrap_or(0);
            let max = device.color_temp_max.unwrap_or(u16::MAX);
            let clamped = if min <= max { ct.clamp(min, max) } else { ct };
            ct_values.push((
                &member.ieee_address,
                cal.apply_color_temp(clamped, Some(device)),
            ));
        }
    }

    fn uniform<T: Copy + PartialEq>(values: &[(&str, T)]) -> Option<T> {
        let first = values.first()?.1;
        values.iter().all(|(_, v)| *v == first).then_some(first)
    }

    let (group_brightness, split_brightness) = match brightness {
        None => (None, false),
        Some(b) if brightness_values.is_empty() => (Some(b), false),
        Some(_) => match uniform(&brightness_values) {
            Some(v) => (Some(v), false),
            None => (None, true),
        },
    };
    let (group_color_temp_mired, split_ct) = match color_temp_mired {
        None => (None, false),
        Some(ct) if ct_values.is_empty() => (Some(ct), false),
        Some(_) => match uniform(&ct_values) {
            Some(v) => (Some(v), false),
            None => (None, true),
        },
    };

    fn lookup<T: Copy>(values: &[(&str, T)], ieee: &str) -> Option<T> {
        values.iter().find(|(i, _)| *i == ieee).map(|(_, v)| *v)
    }

    let members: Vec<MemberPush> = group
        .members
        .iter()
        .filter_map(|m| {
            let ieee = m.ieee_address.as_str();
            let brightness = split_brightness
                .then(|| lookup(&brightness_values, ieee))
                .flatten();
            let color_temp_mired = split_ct.then(|| lookup(&ct_values, ieee)).flatten();
            (brightness.is_some() || color_temp_mired.is_some()).then(|| MemberPush {
                ieee: ieee.to_string(),
                brightness,
                color_temp_mired,
            })
        })
        .collect();

    GroupPublishPlan {
        group_brightness,
        group_color_temp_mired,
        members,
    }
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

    fn group_of(ieees: &[&str]) -> Z2mGroupInfo {
        use crate::state::Z2mGroupMember;
        Z2mGroupInfo {
            id: 1,
            friendly_name: "kitchen".to_string(),
            members: ieees
                .iter()
                .map(|ieee| Z2mGroupMember {
                    ieee_address: ieee.to_string(),
                    endpoint: 1,
                })
                .collect(),
            scenes: vec![],
        }
    }

    fn device(ieee: &str, base: Z2mDeviceInfo) -> (String, Z2mDeviceInfo) {
        (
            ieee.to_string(),
            Z2mDeviceInfo {
                ieee_address: ieee.to_string(),
                ..base
            },
        )
    }

    #[test]
    fn test_plan_uniform_group_is_single_group_command() {
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, Some(153), Some(454))),
            device("0x02", make_device(true, false, Some(153), Some(454))),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02"]),
            &default_config(),
            &device_map,
            Some(207),
            Some(360),
        );
        assert_eq!(plan.group_brightness, Some(207));
        assert_eq!(plan.group_color_temp_mired, Some(360));
        assert!(plan.members.is_empty());
    }

    #[test]
    fn test_plan_rgbw_in_ct_group_never_gets_group_color_temp() {
        // CT-only ceiling bulbs plus one RGBW bulb with the +15 mired auto
        // offset. The group command must not carry color_temp, or the RGBW
        // bulb would be set to 360 and then corrected to 375 on every push.
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, Some(153), Some(454))),
            device("0x02", make_device(true, false, Some(153), Some(454))),
            device("0x03", make_device(true, true, Some(153), Some(500))),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02", "0x03"]),
            &default_config(),
            &device_map,
            Some(207),
            Some(360),
        );
        assert_eq!(plan.group_brightness, Some(207));
        assert_eq!(plan.group_color_temp_mired, None);
        assert_eq!(
            plan.members,
            vec![
                MemberPush {
                    ieee: "0x01".to_string(),
                    brightness: None,
                    color_temp_mired: Some(360),
                },
                MemberPush {
                    ieee: "0x02".to_string(),
                    brightness: None,
                    color_temp_mired: Some(360),
                },
                MemberPush {
                    ieee: "0x03".to_string(),
                    brightness: None,
                    color_temp_mired: Some(375),
                },
            ]
        );
    }

    #[test]
    fn test_plan_clamps_color_temp_per_device() {
        // Target beyond one bulb's range: each bulb gets its own clamped value.
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, Some(153), Some(454))),
            device("0x02", make_device(true, false, Some(153), Some(500))),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02"]),
            &default_config(),
            &device_map,
            None,
            Some(480),
        );
        assert_eq!(plan.group_brightness, None);
        assert_eq!(plan.group_color_temp_mired, None);
        let cts: Vec<_> = plan.members.iter().map(|m| m.color_temp_mired).collect();
        assert_eq!(cts, vec![Some(454), Some(480)]);
    }

    #[test]
    fn test_plan_clamped_but_uniform_stays_in_group() {
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, Some(153), Some(454))),
            device("0x02", make_device(true, false, Some(153), Some(454))),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02"]),
            &default_config(),
            &device_map,
            Some(100),
            Some(500),
        );
        assert_eq!(plan.group_color_temp_mired, Some(454));
        assert!(plan.members.is_empty());
    }

    #[test]
    fn test_plan_brightness_offset_splits_brightness_only() {
        let mut config = default_config();
        config.overrides.insert(
            "0x02".to_string(),
            LightCalibrationOverride {
                color_temp_offset: None,
                brightness_offset: Some(-20),
            },
        );
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, None, None)),
            device("0x02", make_device(true, false, None, None)),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02"]),
            &config,
            &device_map,
            Some(200),
            Some(300),
        );
        assert_eq!(plan.group_brightness, None);
        assert_eq!(plan.group_color_temp_mired, Some(300));
        let brightness: Vec<_> = plan.members.iter().map(|m| m.brightness).collect();
        assert_eq!(brightness, vec![Some(200), Some(180)]);
    }

    #[test]
    fn test_plan_color_temp_ignores_brightness_only_members() {
        let mut dimmer = make_device(false, false, None, None);
        dimmer.supports_brightness = true;
        let device_map: HashMap<_, _> = [
            device("0x01", make_device(true, false, None, None)),
            device("0x02", dimmer),
        ]
        .into();
        let plan = plan_group_publish(
            &group_of(&["0x01", "0x02"]),
            &default_config(),
            &device_map,
            Some(150),
            Some(300),
        );
        assert_eq!(plan.group_brightness, Some(150));
        assert_eq!(plan.group_color_temp_mired, Some(300));
        assert!(plan.members.is_empty());
    }

    #[test]
    fn test_plan_unknown_group_members_fall_back_to_given_values() {
        let plan = plan_group_publish(
            &group_of(&["0x01"]),
            &default_config(),
            &HashMap::new(),
            Some(150),
            Some(300),
        );
        assert_eq!(plan.group_brightness, Some(150));
        assert_eq!(plan.group_color_temp_mired, Some(300));
        assert!(plan.members.is_empty());
    }
}
