use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const P2_MAX_CHUNK: usize = 16 * 1024;
pub const P1_PACE_THRESHOLD: usize = 64 * 1024;
pub const P1_PACE_WINDOW: Duration = Duration::from_millis(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Control acknowledgements and delay probes.
    P0,
    /// Realtime video frames.
    P1,
    /// Background bulk data: clipboard, audio, file transfer.
    P2,
}

#[derive(Debug)]
pub struct Packet {
    pub priority: Priority,
    pub stream: u64,
    pub payload: Vec<u8>,
    pub pushed_at: Instant,
    pub last: bool,
}

struct VideoEntry {
    stream: u64,
    data: Vec<u8>,
    offset: usize,
    pushed_at: Instant,
    ready_at: Instant,
}

#[derive(Default)]
pub struct SendScheduler {
    p0: VecDeque<Packet>,
    p1: VecDeque<VideoEntry>,
    p2: VecDeque<Packet>,
    next_stream: u64,
    framed_p1: VecDeque<Packet>,
    framed_window: Option<Instant>,
    framed_bytes: usize,
}

impl SendScheduler {
    /// Enqueue an already framed protocol message; never split its bytes.
    pub fn push_framed(&mut self, priority: Priority, payload: Vec<u8>, now: Instant) {
        let packet = Packet { priority, stream: 0, payload, pushed_at: now, last: true };
        match priority {
            Priority::P0 => self.p0.push_back(packet),
            Priority::P1 => self.framed_p1.push_back(packet),
            Priority::P2 => self.p2.push_back(packet),
        }
    }

    pub fn push(&mut self, priority: Priority, data: Vec<u8>, now: Instant) -> u64 {
        let stream = self.next_stream;
        self.next_stream += 1;
        match priority {
            Priority::P0 => self.p0.push_back(Packet {
                priority,
                stream,
                payload: data,
                pushed_at: now,
                last: true,
            }),
            Priority::P1 => self.p1.push_back(VideoEntry {
                stream,
                data,
                offset: 0,
                pushed_at: now,
                ready_at: now,
            }),
            Priority::P2 => {
                let chunks = data.len().div_ceil(P2_MAX_CHUNK).max(1);
                for index in 0..chunks {
                    let start = index * P2_MAX_CHUNK;
                    let end = (start + P2_MAX_CHUNK).min(data.len());
                    self.p2.push_back(Packet {
                        priority,
                        stream,
                        payload: data[start..end].to_vec(),
                        pushed_at: now,
                        last: index + 1 == chunks,
                    });
                }
            }
        }
        stream
    }

    pub fn pop(&mut self, now: Instant) -> Option<Packet> {
        self.pop_ready(now, true)
    }

    pub fn has_control(&self) -> bool {
        !self.p0.is_empty()
    }

    pub fn has_framed_video(&self) -> bool {
        !self.framed_p1.is_empty()
    }

    /// When an upstream lane still has unclassified messages, bulk must wait.
    pub fn pop_ready(&mut self, now: Instant, allow_bulk: bool) -> Option<Packet> {
        if let Some(packet) = self.p0.pop_front() {
            return Some(packet);
        }
        if self.framed_window.map_or(true, |at| now >= at + P1_PACE_WINDOW) {
            self.framed_window = Some(now);
            self.framed_bytes = 0;
        }
        if self.framed_bytes < P1_PACE_THRESHOLD {
            if let Some(packet) = self.framed_p1.pop_front() {
                self.framed_bytes += packet.payload.len();
                return Some(packet);
            }
        }
        if let Some(index) = self.p1.iter().position(|entry| entry.ready_at <= now) {
            let entry = &mut self.p1[index];
            let remaining = entry.data.len() - entry.offset;
            let take = if entry.data.len() <= P1_PACE_THRESHOLD {
                remaining
            } else {
                remaining.min(P1_PACE_THRESHOLD)
            };
            let payload = entry.data[entry.offset..entry.offset + take].to_vec();
            entry.offset += take;
            let last = entry.offset >= entry.data.len();
            let (stream, pushed_at) = (entry.stream, entry.pushed_at);
            if last {
                self.p1.remove(index);
            } else {
                entry.ready_at = now + P1_PACE_WINDOW;
            }
            return Some(Packet {
                priority: Priority::P1,
                stream,
                payload,
                pushed_at,
                last,
            });
        }
        if allow_bulk { self.p2.pop_front() } else { None }
    }

    pub fn next_ready_at(&self, now: Instant) -> Option<Instant> {
        self.next_ready_at_with_bulk(now, true)
    }

    pub fn next_ready_at_with_bulk(&self, now: Instant, allow_bulk: bool) -> Option<Instant> {
        if !self.p0.is_empty() || (allow_bulk && !self.p2.is_empty()) {
            return Some(now);
        }
        let framed = if self.framed_p1.is_empty() {
            None
        } else if self.framed_bytes < P1_PACE_THRESHOLD {
            Some(now)
        } else {
            self.framed_window.map(|at| (at + P1_PACE_WINDOW).max(now))
        };
        self.p1.iter().map(|entry| entry.ready_at.max(now)).chain(framed).min()
    }

    pub fn is_empty(&self) -> bool {
        self.p0.is_empty() && self.p1.is_empty() && self.framed_p1.is_empty() && self.p2.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_bulk_wait_does_not_block_control_or_spin_during_video_pacing() {
        let now = Instant::now();
        let mut scheduler = SendScheduler::default();
        scheduler.push_framed(Priority::P2, vec![1], now);
        scheduler.push_framed(Priority::P1, vec![2; P1_PACE_THRESHOLD], now);
        assert_eq!(scheduler.pop_ready(now, false).unwrap().priority, Priority::P1);
        scheduler.push_framed(Priority::P1, vec![3], now);
        scheduler.push_framed(Priority::P0, vec![4], now);
        assert_eq!(scheduler.pop_ready(now, false).unwrap().priority, Priority::P0);
        assert!(scheduler.pop_ready(now, false).is_none());
        assert_eq!(scheduler.next_ready_at_with_bulk(now, false), Some(now + P1_PACE_WINDOW));
        assert_eq!(scheduler.next_ready_at_with_bulk(now, true), Some(now));
        assert_eq!(scheduler.pop_ready(now, true).unwrap().priority, Priority::P2);
    }

    #[test]
    fn framed_packets_keep_boundaries_and_pace_across_messages() {
        let now = Instant::now();
        let mut sched = SendScheduler::default();
        for _ in 0..6 {
            sched.push_framed(Priority::P1, vec![2; 16 * 1024], now);
        }
        sched.push_framed(Priority::P2, vec![1; 16 * 1024 + 64], now);
        for _ in 0..4 {
            assert_eq!(sched.pop(now).unwrap().priority, Priority::P1);
        }
        sched.push_framed(Priority::P0, vec![3], now);
        assert_eq!(sched.pop(now).unwrap().priority, Priority::P0);
        assert_eq!(sched.pop(now).unwrap().payload.len(), 16 * 1024 + 64);
        assert!(sched.pop(now).is_none());
        assert_eq!(sched.next_ready_at(now), Some(now + P1_PACE_WINDOW));
        assert_eq!(sched.pop(now + P1_PACE_WINDOW).unwrap().priority, Priority::P1);
    }

    #[test]
    fn strict_priority_chunking_and_pacing() {
        let now = Instant::now();
        let mut sched = SendScheduler::default();
        sched.push(Priority::P2, vec![1; 40 * 1024], now);
        sched.push(Priority::P1, vec![2; 150 * 1024], now);
        sched.push(Priority::P0, vec![3; 8], now);
        assert_eq!(sched.pop(now).unwrap().priority, Priority::P0);
        let first = sched.pop(now).unwrap();
        assert_eq!((first.priority, first.payload.len(), first.last), (Priority::P1, 64 * 1024, false));
        let bulk = sched.pop(now).unwrap();
        assert_eq!((bulk.priority, bulk.payload.len()), (Priority::P2, P2_MAX_CHUNK));
        assert_eq!(sched.next_ready_at(now), Some(now));
        while sched.pop(now).map_or(false, |p| p.priority == Priority::P2) {}
        assert_eq!(sched.next_ready_at(now), Some(now + P1_PACE_WINDOW));
        assert_eq!(sched.pop(now + P1_PACE_WINDOW).unwrap().payload.len(), 64 * 1024);
    }
}
