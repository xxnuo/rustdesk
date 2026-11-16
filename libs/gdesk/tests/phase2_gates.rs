use gdesk::{
    input::{InputGuard, InputVerdict, MOD_LCTRL},
    sched::{Priority, SendScheduler, P1_PACE_THRESHOLD, P2_MAX_CHUNK},
    wake::{self, INPUT_WAKE_DELAY},
    GDeskAck, GDeskConfig, GDeskDecoder, GDeskEncoder, GDeskFrameHeader, GDeskQos, PixelFormat,
    QoSState,
};
use std::time::{Duration, Instant};

const SIDE: usize = 256;

fn mix(x: usize, y: usize, seed: usize) -> u32 {
    let mut v = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77) ^ (seed as u32).wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^ (v >> 13)
}

fn image(seed: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; SIDE * SIDE * 3];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let n = mix(x, y, seed);
            let at = (y * SIDE + x) * 3;
            rgb[at..at + 3].copy_from_slice(&[(n % 251) as u8, ((n >> 8) % 241) as u8, ((n >> 16) % 239) as u8]);
        }
    }
    rgb
}

fn max_error(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(l, r)| l.abs_diff(*r)).max().unwrap_or(0)
}

struct Link {
    encoder: GDeskEncoder,
    decoder: GDeskDecoder,
    qos: GDeskQos,
    clock: Instant,
    last_seq: u32,
    last_capture_us: u64,
}

impl Link {
    fn new() -> Self {
        Self {
            encoder: GDeskEncoder::new(SIDE as u16, SIDE as u16, GDeskConfig::default()).unwrap(),
            decoder: GDeskDecoder::new(),
            qos: GDeskQos::default(),
            clock: Instant::now(),
            last_seq: 0,
            last_capture_us: 0,
        }
    }

    fn deliver(&mut self, frame: &[u8]) {
        let header = GDeskFrameHeader::from_bytes(frame).unwrap();
        self.last_seq = header.frame_seq;
        self.last_capture_us = header.capture_ts_us;
        self.decoder.decode_frame(frame).unwrap();
    }

    fn client_ack(&mut self, render_queue_depth: u8) {
        let ack = GDeskAck {
            acked_frame_seq: self.last_seq,
            echo_capture_ts_us: self.last_capture_us,
            client_recv_ts_us: 5_000_000 + self.last_capture_us,
            render_queue_depth,
            ..Default::default()
        };
        self.qos.on_ack(&ack, self.clock);
    }
}

#[test]
fn gate1_backpressure_red_light_and_recovery() {
    let mut link = Link::new();
    let mut shown = image(0);
    for seed in 0..3usize {
        shown = image(seed);
        let frame = link
            .encoder
            .encode_frame(&shown, SIDE * 3, PixelFormat::Rgb, (seed as u64 + 1) * 16_000)
            .unwrap()
            .unwrap();
        link.deliver(&frame);
        link.client_ack(0);
    }
    assert_eq!(link.qos.state(link.clock), QoSState::Green);
    assert!(link.encoder.has_pending_refinement(), "changing tiles must leave lossy tiles behind");
    assert!(max_error(link.decoder.canvas_rgb(), &shown) > 0);

    shown = image(3);
    let frame = link
        .encoder
        .encode_frame(&shown, SIDE * 3, PixelFormat::Rgb, 4 * 16_000)
        .unwrap()
        .unwrap();
    link.deliver(&frame);
    link.client_ack(3);
    let observed = link.qos.state(link.clock);
    assert_eq!(observed, QoSState::Red, "one ack round trip must reach the red light");
    link.encoder.set_qos_state(observed);
    assert!(link.qos.should_skip_frame(link.clock), "red light skips one capture");
    assert!(!link.qos.should_skip_frame(link.clock));

    let mut red_bytes = 0;
    for _ in 0..20 {
        if let Some(frame) = link.encoder.poll_pending_refinement().unwrap() {
            red_bytes += frame.len();
        }
    }
    assert_eq!(red_bytes, 0, "no refinement frames may be sent while red");
    assert!(link.encoder.has_pending_refinement());

    shown = image(4);
    let frame = link
        .encoder
        .encode_frame(&shown, SIDE * 3, PixelFormat::Rgb, 5 * 16_000)
        .unwrap()
        .unwrap();
    assert_eq!(GDeskFrameHeader::from_bytes(&frame).unwrap().qos_state, QoSState::Red);
    link.deliver(&frame);
    link.client_ack(0);
    assert_eq!(link.qos.state(link.clock), QoSState::Green);
    link.encoder.set_qos_state(QoSState::Green);
    let mut refinement_bytes = 0;
    for _ in 0..20 {
        match link.encoder.poll_pending_refinement().unwrap() {
            Some(frame) => {
                refinement_bytes += frame.len();
                link.deliver(&frame);
            }
            None if !link.encoder.has_pending_refinement() => break,
            None => {}
        }
    }
    let error = max_error(link.decoder.canvas_rgb(), &shown);
    println!("GATE1 red_bytes={red_bytes} refinement_bytes={refinement_bytes} max_error={error}");
    assert!(refinement_bytes > 0);
    assert!(link.decoder.is_lossless());
    assert_eq!(error, 0);
}

#[test]
fn gate2_stuck_key_auto_release_and_lease() {
    let mut guard = InputGuard::default();
    let lease = 41;
    assert_eq!(
        guard.check(lease, MOD_LCTRL, Some((MOD_LCTRL, true))),
        InputVerdict::Accept { release: 0 }
    );
    assert_eq!(guard.pressed(), MOD_LCTRL);

    let verdict = guard.check(lease, 0, None);
    println!("GATE2 mouse move with empty bitmap -> {verdict:?}");
    assert_eq!(verdict, InputVerdict::Accept { release: MOD_LCTRL });
    assert_eq!(guard.pressed(), 0);

    assert_eq!(
        guard.check(lease, MOD_LCTRL, Some((MOD_LCTRL, true))),
        InputVerdict::Accept { release: 0 }
    );
    assert_eq!(guard.check(lease - 1, MOD_LCTRL, None), InputVerdict::Reject);
    assert_eq!(guard.pressed(), MOD_LCTRL, "a rejected packet must not touch key state");
    assert_eq!(guard.rotate_lease(lease + 1), MOD_LCTRL);
    assert_eq!(guard.check(lease, 0, None), InputVerdict::Reject);
    assert_eq!(guard.check(lease + 1, 0, None), InputVerdict::Accept { release: 0 });

    let notifier = wake::global();
    notifier.wait_timeout(Duration::from_millis(1));
    let started = Instant::now();
    notifier.notify_after(INPUT_WAKE_DELAY);
    assert!(notifier.wait_timeout(Duration::from_millis(100)));
    let woke_after = started.elapsed();
    println!("GATE2 wake_after_us={}", woke_after.as_micros());
    assert!(woke_after >= Duration::from_micros(1000) && woke_after < Duration::from_millis(20));
}

struct Percentiles(Vec<Duration>);

impl Percentiles {
    fn p99(&mut self) -> Duration {
        self.0.sort();
        self.0[(self.0.len() * 99 / 100).min(self.0.len() - 1)]
    }
}

fn simulate(with_bulk: bool) -> (Percentiles, Percentiles, usize, usize) {
    const NS_PER_BYTE: u64 = 8;
    const BULK: usize = 50 * 1024 * 1024;
    let base = Instant::now();
    let at = |nanos: u64| base + Duration::from_nanos(nanos);
    let mut sched = SendScheduler::default();
    if with_bulk {
        sched.push(Priority::P2, vec![7; BULK], base);
    }
    let (mut feedback, mut video) = (Percentiles(vec![]), Percentiles(vec![]));
    let (mut bulk_bytes, mut biggest_chunk) = (0usize, 0usize);
    let (mut next_click, mut next_update, mut next_big) = (0u64, 0u64, 0u64);
    let mut small_streams = std::collections::HashSet::new();
    let mut now = 0u64;
    let end = 600_000_000u64;
    while now < end {
        while next_click <= now {
            sched.push(Priority::P0, vec![1; 64], at(next_click));
            next_click += 1_000_000;
        }
        while next_update <= now {
            small_streams.insert(sched.push(Priority::P1, vec![2; 12 * 1024], at(next_update)));
            next_update += 4_000_000;
        }
        while next_big <= now {
            sched.push(Priority::P1, vec![3; 256 * 1024], at(next_big));
            next_big += 100_000_000;
        }
        match sched.pop(at(now)) {
            Some(packet) => {
                let waited = at(now) - packet.pushed_at;
                match packet.priority {
                    Priority::P0 => feedback.0.push(waited),
                    Priority::P1 if small_streams.contains(&packet.stream) => video.0.push(waited),
                    Priority::P1 => assert!(packet.payload.len() <= P1_PACE_THRESHOLD),
                    Priority::P2 => {
                        bulk_bytes += packet.payload.len();
                        biggest_chunk = biggest_chunk.max(packet.payload.len());
                    }
                }
                now += packet.payload.len() as u64 * NS_PER_BYTE;
            }
            None => {
                let wake_at = sched
                    .next_ready_at(at(now))
                    .map(|t| (t - base).as_nanos() as u64)
                    .unwrap_or(u64::MAX);
                now = wake_at.min(next_click).min(next_update).min(next_big).max(now + 1);
            }
        }
    }
    (feedback, video, bulk_bytes, biggest_chunk)
}

#[test]
fn gate3_bulk_50mb_clipboard_priority_isolation() {
    let (mut base_feedback, mut base_video, _, _) = simulate(false);
    let (mut feedback, mut video, bulk_bytes, biggest_chunk) = simulate(true);
    let feedback_delta = feedback.p99().saturating_sub(base_feedback.p99());
    let video_delta = video.p99().saturating_sub(base_video.p99());
    println!(
        "GATE3 bulk_bytes={bulk_bytes} biggest_chunk={biggest_chunk} feedback_p99_delta_us={} video_p99_delta_us={}",
        feedback_delta.as_micros(),
        video_delta.as_micros()
    );
    assert_eq!(bulk_bytes, 50 * 1024 * 1024, "the whole clipboard payload must still be delivered");
    assert!(biggest_chunk <= P2_MAX_CHUNK);
    assert!(feedback_delta < Duration::from_micros(3000));
    assert!(video_delta < Duration::from_micros(3000));
}
