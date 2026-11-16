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
}

impl SendScheduler {
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
        if let Some(packet) = self.p0.pop_front() {
            return Some(packet);
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
        self.p2.pop_front()
    }

    pub fn next_ready_at(&self, now: Instant) -> Option<Instant> {
        if !self.p0.is_empty() || !self.p2.is_empty() {
            return Some(now);
        }
        self.p1.iter().map(|entry| entry.ready_at.max(now)).min()
    }

    pub fn is_empty(&self) -> bool {
        self.p0.is_empty() && self.p1.is_empty() && self.p2.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
