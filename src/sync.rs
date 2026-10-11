use std::time::{Duration, Instant};

use crate::denon::{ReceiverStatus, StatusFields};

const PENDING_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AcceptedStatus {
    pub volume_code: Option<f64>,
    pub muted: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Reconciler {
    desired_volume: Option<i32>,
    desired_mute: Option<bool>,
    volume_pending_since: Option<Instant>,
    mute_pending_since: Option<Instant>,
}

impl Reconciler {
    pub fn request_volume(&mut self, code: i32, now: Instant) {
        self.desired_volume = Some(code);
        self.volume_pending_since = Some(now);
    }

    pub fn request_mute(&mut self, muted: bool, now: Instant) {
        self.desired_mute = Some(muted);
        self.mute_pending_since = Some(now);
    }

    pub fn desired_volume(&self) -> Option<i32> {
        self.desired_volume
    }

    pub fn desired_mute(&self) -> Option<bool> {
        self.desired_mute
    }

    pub fn accept_status(
        &mut self,
        status: &ReceiverStatus,
        changed: StatusFields,
        now: Instant,
    ) -> AcceptedStatus {
        let volume_code = if changed.contains(StatusFields::VOLUME) {
            if let Some(since) = self.volume_pending_since {
                if now.duration_since(since) <= PENDING_TIMEOUT {
                    if status.volume_code.round() as i32 != self.desired_volume.unwrap_or_default()
                    {
                        None
                    } else {
                        self.volume_pending_since = None;
                        (self.desired_volume == Some(status.volume_code.round() as i32))
                            .then_some(status.volume_code)
                    }
                } else {
                    self.volume_pending_since = None;
                    self.desired_volume = Some(status.volume_code.round() as i32);
                    Some(status.volume_code)
                }
            } else {
                self.desired_volume = Some(status.volume_code.round() as i32);
                Some(status.volume_code)
            }
        } else {
            None
        };

        let muted = if changed.contains(StatusFields::MUTE) {
            if let Some(since) = self.mute_pending_since {
                if now.duration_since(since) <= PENDING_TIMEOUT {
                    if status.muted != self.desired_mute.unwrap_or(status.muted) {
                        None
                    } else {
                        self.mute_pending_since = None;
                        (self.desired_mute == Some(status.muted)).then_some(status.muted)
                    }
                } else {
                    self.mute_pending_since = None;
                    self.desired_mute = Some(status.muted);
                    Some(status.muted)
                }
            } else {
                self.desired_mute = Some(status.muted);
                Some(status.muted)
            }
        } else {
            None
        };

        AcceptedStatus { volume_code, muted }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume_status(code: f64) -> ReceiverStatus {
        ReceiverStatus {
            volume_code: code,
            ..ReceiverStatus::default()
        }
    }

    #[test]
    fn stale_volume_echo_does_not_replace_newer_request() {
        let start = Instant::now();
        let mut reconciler = Reconciler::default();
        reconciler.request_volume(7, start);
        reconciler.request_volume(22, start + Duration::from_millis(100));

        let stale = reconciler.accept_status(
            &volume_status(7.0),
            StatusFields::VOLUME,
            start + Duration::from_millis(250),
        );
        assert_eq!(stale.volume_code, None);
        assert_eq!(reconciler.desired_volume(), Some(22));

        let acknowledged = reconciler.accept_status(
            &volume_status(22.0),
            StatusFields::VOLUME,
            start + Duration::from_millis(400),
        );
        assert_eq!(acknowledged.volume_code, Some(22.0));
        assert_eq!(reconciler.desired_volume(), Some(22));
    }

    #[test]
    fn timed_out_pending_status_is_accepted_as_authoritative() {
        let start = Instant::now();
        let mut reconciler = Reconciler::default();
        reconciler.request_volume(22, start);

        let accepted = reconciler.accept_status(
            &volume_status(18.0),
            StatusFields::VOLUME,
            start + PENDING_TIMEOUT + Duration::from_millis(1),
        );
        assert_eq!(accepted.volume_code, Some(18.0));
        assert_eq!(reconciler.desired_volume(), Some(18));
    }

    #[test]
    fn mute_echoes_are_reconciled_independently() {
        let start = Instant::now();
        let mut reconciler = Reconciler::default();
        reconciler.request_mute(true, start);
        let mut status = ReceiverStatus {
            muted: false,
            ..ReceiverStatus::default()
        };
        assert_eq!(
            reconciler
                .accept_status(&status, StatusFields::MUTE, start)
                .muted,
            None
        );
        status.muted = true;
        assert_eq!(
            reconciler
                .accept_status(
                    &status,
                    StatusFields::MUTE,
                    start + Duration::from_millis(10)
                )
                .muted,
            Some(true)
        );
        assert_eq!(reconciler.desired_mute(), Some(true));
    }
}
