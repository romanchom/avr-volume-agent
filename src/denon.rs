use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReceiverStatus {
    pub max_code: f64,
    pub volume_code: f64,
    pub muted: bool,
    pub seen: StatusFields,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusFields(u8);

impl StatusFields {
    pub const VOLUME: Self = Self(1 << 0);
    pub const MAX_VOLUME: Self = Self(1 << 1);
    pub const MUTE: Self = Self(1 << 2);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn insert(&mut self, fields: Self) {
        self.0 |= fields.0;
    }
}

impl std::ops::BitOr for StatusFields {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

pub fn parse_line(line: &str, status: &mut ReceiverStatus) -> StatusFields {
    let mut changed = StatusFields::default();

    if let Some(value) = line.strip_prefix("MVMAX ") {
        if let Some(parsed) = parse_denon_code(value, 98.0) {
            status.max_code = parsed;
            changed.insert(StatusFields::MAX_VOLUME);
        }
    } else if let Some(value) = line.strip_prefix("MV") {
        if let Some(parsed) = parse_denon_code(value, 98.0) {
            status.volume_code = parsed;
            changed.insert(StatusFields::VOLUME);
        }
    } else if line == "MUON" {
        status.muted = true;
        changed.insert(StatusFields::MUTE);
    } else if line == "MUOFF" {
        status.muted = false;
        changed.insert(StatusFields::MUTE);
    }

    status.seen.insert(changed);
    changed
}

fn parse_denon_code(value: &str, max: f64) -> Option<f64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    let parsed = u16::from_str(value).ok()?;
    let code = match value.len() {
        1..=2 if parsed <= max as u16 => f64::from(parsed),
        3 if parsed <= 980 => f64::from(parsed) / 10.0,
        _ => return None,
    };

    (code <= max).then_some(code)
}

pub fn slider_to_code(channel_volume: f32, max_code: f64) -> i32 {
    if max_code <= 0.0 {
        return 0;
    }
    let volume = channel_volume.clamp(0.0, 1.0);
    ((volume.cbrt() as f64 * max_code).round() as i32).clamp(0, max_code.floor() as i32)
}

pub fn code_to_channel_volume(volume_code: f64, max_code: f64) -> f32 {
    if max_code <= 0.0 {
        return 0.0;
    }
    (volume_code / max_code).clamp(0.0, 1.0).powi(3) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_volume_mute_and_maximum_status() {
        let mut status = ReceiverStatus::default();
        assert_eq!(parse_line("MV44", &mut status), StatusFields::VOLUME);
        assert_eq!(status.volume_code, 44.0);
        assert_eq!(parse_line("MV445", &mut status), StatusFields::VOLUME);
        assert_eq!(status.volume_code, 44.5);
        assert_eq!(
            parse_line("MVMAX 72", &mut status),
            StatusFields::MAX_VOLUME
        );
        assert_eq!(status.max_code, 72.0);
        assert_eq!(
            parse_line("MVMAX 715", &mut status),
            StatusFields::MAX_VOLUME
        );
        assert_eq!(status.max_code, 71.5);
        assert_eq!(parse_line("MUOFF", &mut status), StatusFields::MUTE);
        assert!(!status.muted);
        assert_eq!(parse_line("MUON", &mut status), StatusFields::MUTE);
        assert!(status.muted);
        assert!(
            status
                .seen
                .contains(StatusFields::VOLUME | StatusFields::MAX_VOLUME | StatusFields::MUTE)
        );
    }

    #[test]
    fn ignores_invalid_and_unrelated_lines() {
        let mut status = ReceiverStatus {
            max_code: 72.0,
            volume_code: 44.0,
            ..ReceiverStatus::default()
        };
        for line in ["MVMAX 99", "MV99", "MV?", "SISAT/CBL", "MV44junk"] {
            assert_eq!(parse_line(line, &mut status), StatusFields::default());
        }
        assert_eq!(status.max_code, 72.0);
        assert_eq!(status.volume_code, 44.0);
    }

    #[test]
    fn maps_pipewire_linear_volume_to_denon_code() {
        assert_eq!(slider_to_code(0.0, 72.0), 0);
        assert_eq!(slider_to_code(1.0, 72.0), 72);
        assert_eq!(slider_to_code(0.001, 72.0), 7);
        assert_eq!(slider_to_code(0.216, 72.0), 43);
        assert_eq!(slider_to_code(0.216, 0.0), 0);
        assert!((code_to_channel_volume(44.0, 72.0) - 0.2286).abs() < 0.001);
        assert_eq!(code_to_channel_volume(44.0, 0.0), 0.0);
    }
}
