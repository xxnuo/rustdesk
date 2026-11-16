use crate::{GDeskAck, QoSState};
use std::time::{Duration, Instant};

pub const GDESK_FEEDBACK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default)]
pub struct GDeskQos {
    feedback_state: QoSState,
    previous_ack: Option<(u32, u64, u64)>,
    last_feedback: Option<Instant>,
    send_backlog: bool,
    skip_next: bool,
}
impl GDeskQos {
    pub fn on_ack(&mut self, ack: &GDeskAck, now: Instant) {
        self.expire(now);
        if self
            .previous_ack
            .is_some_and(|(seq, _, _)| ack.acked_frame_seq <= 2 && ack.acked_frame_seq < seq)
        {
            self.previous_ack = None;
        }
        let previous = self.previous_ack;
        if previous.is_some_and(|(seq, _, _)| ack.acked_frame_seq.wrapping_sub(seq) as i32 <= 0) {
            return;
        }
        let old_state = self.current_state();
        let gradient = previous.map_or(0, |(_, sent, received)| {
            if ack.echo_capture_ts_us <= sent || ack.client_recv_ts_us < received {
                return 0;
            }
            (ack.client_recv_ts_us - received) as i128 - (ack.echo_capture_ts_us - sent) as i128
        });
        self.feedback_state = if ack.render_queue_depth >= 2 || gradient > 20_000 {
            QoSState::Red
        } else if ack.render_queue_depth == 1 || gradient >= 5_000 {
            QoSState::Yellow
        } else {
            QoSState::Green
        };
        // Refinement reuses the capture timestamp; it is not a new latency sample.
        let (sent, received) = match previous {
            Some((_, sent, received)) if ack.echo_capture_ts_us == sent => (sent, received),
            _ => (ack.echo_capture_ts_us, ack.client_recv_ts_us),
        };
        self.previous_ack = Some((ack.acked_frame_seq, sent, received));
        self.last_feedback = Some(now);
        self.arm_skip(old_state);
    }

    pub fn set_send_backlog(&mut self, backlogged: bool, now: Instant) {
        self.expire(now);
        let old_state = self.current_state();
        self.send_backlog = backlogged;
        self.arm_skip(old_state);
    }

    pub fn state(&mut self, now: Instant) -> QoSState {
        self.expire(now);
        self.current_state()
    }

    pub fn should_skip_frame(&mut self, now: Instant) -> bool {
        self.expire(now);
        self.current_state() == QoSState::Red && std::mem::take(&mut self.skip_next)
    }

    fn current_state(&self) -> QoSState {
        if self.send_backlog {
            QoSState::Red
        } else {
            self.feedback_state
        }
    }
    fn arm_skip(&mut self, previous: QoSState) {
        if self.current_state() == QoSState::Red {
            if previous != QoSState::Red {
                self.skip_next = true;
            }
        } else {
            self.skip_next = false;
        }
    }
    fn expire(&mut self, now: Instant) {
        if self
            .last_feedback
            .is_some_and(|at| now.saturating_duration_since(at) >= GDESK_FEEDBACK_TIMEOUT)
        {
            self.feedback_state = QoSState::Green;
            self.previous_ack = None;
            self.last_feedback = None;
            if !self.send_backlog {
                self.skip_next = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ack(seq: u32, sent: u64, received: u64, depth: u8) -> GDeskAck {
        GDeskAck {
            acked_frame_seq: seq,
            echo_capture_ts_us: sent,
            client_recv_ts_us: received,
            render_queue_depth: depth,
            ..Default::default()
        }
    }
    #[test]
    fn encoder_switch_restarts_ack_baseline_immediately() {
        let now = Instant::now();
        let mut qos = GDeskQos::default();
        qos.on_ack(&ack(100, 10_000, 90_000, 3), now);
        assert_eq!(qos.state(now), QoSState::Red);
        qos.on_ack(&ack(1, 20_000, 100_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Green);
        qos.on_ack(&ack(2, 30_000, 115_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Yellow);
    }

    #[test]
    fn gradient_cancels_clock_offset_and_queue_recovers() {
        let now = Instant::now();
        let mut qos = GDeskQos::default();
        qos.on_ack(&ack(1, 100_000, 9_100_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Green);
        qos.on_ack(&ack(2, 116_000, 9_121_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Yellow);
        qos.on_ack(&ack(3, 132_000, 9_157_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Yellow);
        qos.on_ack(&ack(4, 148_000, 9_173_000, 3), now);
        assert_eq!(qos.state(now), QoSState::Red);
        assert!(qos.should_skip_frame(now));
        assert!(!qos.should_skip_frame(now));
        qos.on_ack(&ack(5, 164_000, 9_189_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Green);
        qos.on_ack(&ack(6, 180_000, 9_205_000, 1), now);
        assert_eq!(qos.state(now), QoSState::Yellow);
        assert_eq!(qos.state(now + GDESK_FEEDBACK_TIMEOUT), QoSState::Green);
    }
    #[test]
    fn backlog_and_stale_feedback_do_not_undo_backpressure() {
        let now = Instant::now();
        let mut qos = GDeskQos::default();
        qos.on_ack(&ack(u32::MAX, 100, 900, 2), now);
        qos.on_ack(&ack(u32::MAX - 1, 1, 1, 0), now);
        assert_eq!(qos.state(now), QoSState::Red);
        qos.on_ack(&ack(0, 200, 1000, 0), now);
        assert_eq!(qos.state(now), QoSState::Green);
        qos.set_send_backlog(true, now);
        assert!(qos.should_skip_frame(now));
        qos.on_ack(&ack(1, 300, 1100, 0), now);
        assert_eq!(qos.state(now + GDESK_FEEDBACK_TIMEOUT), QoSState::Red);
        qos.set_send_backlog(false, now + GDESK_FEEDBACK_TIMEOUT);
        assert_eq!(qos.state(now + GDESK_FEEDBACK_TIMEOUT), QoSState::Green);
    }
    #[test]
    fn refinement_is_not_a_new_capture_latency_sample() {
        let now = Instant::now();
        let mut qos = GDeskQos::default();
        qos.on_ack(&ack(1, 100_000, 1_100_000, 0), now);
        qos.on_ack(&ack(2, 100_000, 1_150_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Green);
        qos.on_ack(&ack(3, 200_000, 1_230_000, 0), now);
        assert_eq!(qos.state(now), QoSState::Red);
        assert_eq!(qos.state(now + GDESK_FEEDBACK_TIMEOUT), QoSState::Green);
    }
}
