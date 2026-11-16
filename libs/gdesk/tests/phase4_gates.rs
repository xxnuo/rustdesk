use gdesk::{
    input::InputGuard,
    udp::{
        is_virtual_nic, pacing_offsets_us, sort_candidate_interfaces, InputRedundancyRx,
        InputRedundancyTx, GDESK_UDP_MTU,
    },
    GDeskConfig, GDeskDecoder, GDeskEncoder, PixelFormat,
};
use std::{cmp::Reverse, collections::BinaryHeap};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
    fn chance(&mut self, probability: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < probability
    }
}

const LOSS: f64 = 0.15;
const ONE_WAY_US: u64 = 40_000;
const JITTER_US: u64 = 20_000;

struct Channel {
    rng: Rng,
    queue: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>>,
    order: u64,
    sent: u64,
    dropped: u64,
}

impl Channel {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng(seed),
            queue: BinaryHeap::new(),
            order: 0,
            sent: 0,
            dropped: 0,
        }
    }
    fn send(&mut self, now_us: u64, data: Vec<u8>) {
        self.sent += 1;
        if self.rng.chance(LOSS) {
            self.dropped += 1;
            return;
        }
        let jitter = self.rng.next() % (JITTER_US + 1);
        let arrival = now_us + ONE_WAY_US - JITTER_US / 2 + jitter;
        self.order += 1;
        self.queue.push(Reverse((arrival, self.order, data)));
    }
    fn receive(&mut self, now_us: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.queue.peek().is_some_and(|Reverse((at, _, _))| *at <= now_us) {
            if let Some(Reverse((_, _, data))) = self.queue.pop() {
                out.push(data);
            }
        }
        out
    }
}

#[test]
fn gate1_500_keys_over_15_percent_loss_80ms_rtt_20ms_jitter() {
    for seed in 1..=10u64 {
        let mut channel = Channel::new(seed);
        let mut tx = InputRedundancyTx::<u32>::new(42);
        let mut rx = InputRedundancyRx::new(InputGuard::default());
        let mut executed = Vec::new();
        let mut pending: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut now_us = 0u64;
        let mut next_key = 0u32;
        let end_us = 500 * 10_000 + 400_000;
        while now_us <= end_us {
            if now_us % 10_000 == 0 && next_key < 500 {
                let packets = tx.push_with_redundancy(0, next_key, 2).unwrap();
                next_key += 1;
                for (copy, packet) in packets.into_iter().enumerate() {
                    pending.push((now_us + copy as u64 * 3_000, packet));
                }
            }
            if next_key == 500 && now_us % 10_000 == 0 && (500 * 10_000 + 10_000..500 * 10_000 + 40_000).contains(&now_us) {
                for packet in tx.flush_tail(1).unwrap() {
                    pending.push((now_us, packet));
                }
            }
            let mut index = 0;
            while index < pending.len() {
                if pending[index].0 <= now_us {
                    let (_, packet) = pending.swap_remove(index);
                    channel.send(now_us, packet);
                } else {
                    index += 1;
                }
            }
            for packet in channel.receive(now_us) {
                executed.extend(rx.receive(&packet).unwrap());
            }
            now_us += 1_000;
        }
        let seqs: Vec<u32> = executed.iter().map(|event| event.seq).collect();
        let duplicates = seqs.windows(2).filter(|pair| pair[0] >= pair[1]).count();
        println!(
            "GATE1 seed={seed} executed={} lost={} duplicates={duplicates} packets={} dropped={}",
            rx.executed, rx.lost, channel.sent, channel.dropped
        );
        assert!(channel.dropped > 100, "the simulator must drop packets");
        assert_eq!((rx.executed, rx.lost, rx.rejected, duplicates), (500, 0, 0, 0));
        assert_eq!(seqs, (1..=500).collect::<Vec<_>>());
        for (event, key) in executed.iter().zip(0u32..) {
            assert_eq!(event.payload, key.to_le_bytes());
        }
    }
}

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

fn mix(x: usize, y: usize) -> u32 {
    let mut value = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
    value ^= value >> 15;
    value = value.wrapping_mul(0x2C1B_3C6D);
    value ^ (value >> 13)
}

fn desktop(window_x: usize, window_y: usize) -> Vec<u8> {
    let mut rgba = vec![255u8; WIDTH * HEIGHT * 4];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let mut pixel = [30u8, 34, 46];
            if x < 192 {
                pixel = if (x / 2 + y / 16) % 5 == 0 { [220, 220, 220] } else { [40, 44, 60] };
            }
            if y >= 960 {
                let noise = (mix(x, y) % 3) as u8;
                pixel = [(x * 255 / WIDTH) as u8, 90u8.wrapping_add(noise), (255 - x * 255 / WIDTH) as u8];
            }
            if (window_x..window_x + 512).contains(&x) && (window_y..window_y + 384).contains(&y) {
                let (u, v) = (x - window_x, y - window_y);
                pixel = if v < 24 {
                    [60, 90, 200]
                } else if u < 256 {
                    if (u / 3 + v / 8) % 4 == 0 { [10, 10, 10] } else { [245, 245, 245] }
                } else {
                    let noise = (mix(u, v) % 2) as u8;
                    [((u * 2) % 256) as u8, ((v * 2) % 256) as u8, ((u + v) as u8).wrapping_add(noise)]
                };
            }
            let at = (y * WIDTH + x) * 4;
            rgba[at..at + 3].copy_from_slice(&pixel);
        }
    }
    rgba
}

fn max_error(canvas_rgb: &[u8], truth_rgba: &[u8]) -> u8 {
    assert_eq!(canvas_rgb.len() / 3, truth_rgba.len() / 4);
    canvas_rgb
        .chunks_exact(3)
        .zip(truth_rgba.chunks_exact(4))
        .map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0))
        .max()
        .unwrap_or(0)
}

fn tile_equals(canvas_rgb: &[u8], reference_rgba: &[u8], tile_x: usize, tile_y: usize) -> bool {
    for y in tile_y * 64..((tile_y + 1) * 64).min(HEIGHT) {
        for x in tile_x * 64..((tile_x + 1) * 64).min(WIDTH) {
            let at = y * WIDTH + x;
            for channel in 0..3 {
                if canvas_rgb[at * 3 + channel] != reference_rgba[at * 4 + channel] {
                    return false;
                }
            }
        }
    }
    true
}

struct DragResult {
    converge_ms: Option<u64>,
    datagrams: usize,
    largest: usize,
    dropped: u64,
    corrupt_tiles: usize,
    final_error: u8,
    unrefinable: u64,
    unconfirmed: usize,
}

fn run_drag(seed: u64, copies: usize) -> DragResult {
    const DRAG_FRAMES: u64 = 24;
    let mut data = Channel::new(seed);
    let mut acks = Channel::new(seed ^ 0xA5A5);
    let config = GDeskConfig {
        refine_min_still_frames: 1,
        require_confirmed_cache_slots: true,
        udp_redundancy_factor: 3,
        udp_refine_redundancy_factor: copies as u8,
        ..Default::default()
    };
    let mut encoder = GDeskEncoder::new(WIDTH as u16, HEIGHT as u16, config).unwrap();
    let mut decoder = GDeskDecoder::new();
    let mut last_arrival: Vec<(u32, u64)> = Vec::new();
    let mut outbox: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut result = DragResult {
        converge_ms: None,
        datagrams: 0,
        largest: 0,
        dropped: 0,
        corrupt_tiles: 0,
        final_error: 255,
        unrefinable: 0,
        unconfirmed: 0,
    };
    let drag_end_us = (DRAG_FRAMES - 1) * 16_000;
    let background = desktop(WIDTH, HEIGHT);
    let mut truth;
    let mut truth_final = Vec::new();
    let cols = WIDTH.div_ceil(64);
    let untouched: Vec<(usize, usize)> = (0..14)
        .flat_map(|row| (0..cols).map(move |col| (col, row)))
        .filter(|(col, _)| *col < 3 || *col >= 26)
        .collect();
    let mut now_us = 0u64;
    while now_us <= drag_end_us + 1_500_000 && result.converge_ms.is_none() {
        let datagrams = if now_us % 16_000 == 0 && now_us / 16_000 < DRAG_FRAMES {
            let step = (now_us / 16_000) as usize;
            truth = desktop(200 + step * 40, 120 + step * 12);
            let out = encoder
                .encode_udp_datagrams(&truth, WIDTH * 4, PixelFormat::Rgba, now_us)
                .unwrap();
            if now_us == drag_end_us {
                truth_final = truth.clone();
            }
            out
        } else {
            encoder.poll_refinement_udp_datagrams(now_us).unwrap()
        };
        if !datagrams.is_empty() {
            let lens: Vec<usize> = datagrams.iter().map(Vec::len).collect();
            let offsets = pacing_offsets_us(&lens);
            for (datagram, offset) in datagrams.into_iter().zip(offsets) {
                assert!(datagram.len() <= GDESK_UDP_MTU, "{} bytes", datagram.len());
                result.datagrams += 1;
                result.largest = result.largest.max(datagram.len());
                outbox.push((now_us + offset, datagram));
            }
        }
        let mut index = 0;
        while index < outbox.len() {
            if outbox[index].0 <= now_us {
                let (_, datagram) = outbox.swap_remove(index);
                data.send(now_us, datagram);
            } else {
                index += 1;
            }
        }
        for datagram in data.receive(now_us) {
            if decoder.decode_udp_datagram(&datagram).is_ok() {
                let seq = u32::from_le_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]);
                match last_arrival.iter_mut().find(|(s, _)| *s == seq) {
                    Some(entry) => entry.1 = now_us,
                    None => last_arrival.push((seq, now_us)),
                }
            }
        }
        let mut ready = Vec::new();
        last_arrival.retain(|(seq, at)| {
            if now_us >= *at + 16_000 {
                ready.push(*seq);
                false
            } else {
                true
            }
        });
        for seq in ready {
            let mut message = seq.to_le_bytes().to_vec();
            message.extend(decoder.tile_ack_bitmap(seq));
            acks.send(now_us, message);
        }
        for message in acks.receive(now_us) {
            let seq = u32::from_le_bytes([message[0], message[1], message[2], message[3]]);
            encoder.apply_tile_ack_bitmap(seq, &message[4..]).unwrap();
        }
        if now_us % 16_000 == 0 && decoder.dimensions().is_some() {
            let canvas = decoder.canvas_rgb();
            for (col, row) in &untouched {
                if decoder.udp_tile_received(row * cols + col)
                    && !tile_equals(canvas, &background, *col, *row)
                {
                    result.corrupt_tiles += 1;
                }
            }
        }
        if now_us >= drag_end_us && decoder.is_lossless() && !truth_final.is_empty() {
            let error = max_error(decoder.canvas_rgb(), &truth_final);
            if error == 0 {
                result.converge_ms = Some((now_us - drag_end_us) / 1_000);
            }
            result.final_error = error;
        }
        now_us += 1_000;
    }
    result.dropped = data.dropped;
    result.unrefinable = encoder.udp.unrefinable_tiles;
    result.unconfirmed = encoder.udp.unconfirmed_tiles();
    result
}

#[test]
fn gate2_window_drag_then_static_converges_lossless_under_200ms() {
    for seed in 1..=4u64 {
        let result = run_drag(seed, 6);
        println!(
            "GATE2 seed={seed} datagrams={} largest={}B dropped={} corrupt_tiles={} converge_ms_after_drag={:?} final_error={} unrefinable={} unconfirmed={}",
            result.datagrams, result.largest, result.dropped, result.corrupt_tiles, result.converge_ms, result.final_error, result.unrefinable, result.unconfirmed
        );
        assert!(result.dropped > 50, "the simulator must drop datagrams");
        assert!(result.largest <= GDESK_UDP_MTU);
        assert_eq!(result.corrupt_tiles, 0, "untouched tiles must never corrupt");
        let converged = result.converge_ms.expect("canvas must converge");
        assert_eq!(result.final_error, 0);
        assert!(converged < 200, "converged after {converged} ms");
    }
}

#[test]
fn gate3_virtual_nics_sort_after_physical_interfaces() {
    let mut names = vec![
        "Docker", "eth0", "WSL", "vEthernet (Default Switch)", "wlan0", "ZeroTier One", "en0",
        "Tailscale", "Ethernet", "utun0", "tun0",
    ];
    sort_candidate_interfaces(&mut names, |name| name);
    assert_eq!(
        names,
        [
            "eth0", "wlan0", "en0", "Ethernet", "Docker", "WSL", "vEthernet (Default Switch)",
            "ZeroTier One", "Tailscale", "utun0", "tun0"
        ]
    );
    assert!(["docker0", "virbr0", "veth1a2b", "vmnet8", "vboxnet0", "tap0"]
        .iter()
        .all(|name| is_virtual_nic(name)));
    assert!(!["eth0", "wlan0", "en0", "Ethernet"].iter().any(|name| is_virtual_nic(name)));
}

fn exchange(
    encoder: &mut GDeskEncoder,
    decoder: &mut GDeskDecoder,
    datagrams: &[Vec<u8>],
    drop_every: Option<usize>,
) -> usize {
    let mut cache_hits = 0;
    let mut seqs = Vec::new();
    let mut delivered = Vec::new();
    for (idx, datagram) in datagrams.iter().enumerate() {
        assert!(datagram.len() <= GDESK_UDP_MTU);
        let (header, records) = gdesk::udp::unpack_udp_datagram(datagram).unwrap();
        cache_hits += records
            .iter()
            .filter(|record| matches!(record, gdesk::protocol::TileRecord::CacheHit { .. }))
            .count();
        if !seqs.contains(&header.frame_seq) {
            seqs.push(header.frame_seq);
        }
        if drop_every.is_some_and(|n| idx % n == 1) {
            continue;
        }
        delivered.push(datagram);
    }
    delivered.reverse();
    for datagram in delivered {
        decoder.decode_udp_datagram(datagram).unwrap();
    }
    for seq in seqs {
        encoder.apply_tile_ack_bitmap(seq, &decoder.tile_ack_bitmap(seq)).unwrap();
    }
    cache_hits
}

fn settle(encoder: &mut GDeskEncoder, decoder: &mut GDeskDecoder, now_us: &mut u64) {
    for _ in 0..400 {
        *now_us += 1_000;
        let datagrams = encoder.poll_refinement_udp_datagrams(*now_us).unwrap();
        exchange(encoder, decoder, &datagrams, None);
        if decoder.is_lossless() && !encoder.has_udp_unconfirmed() {
            return;
        }
    }
    panic!("session did not settle");
}

#[test]
fn confirmed_lossless_tiles_come_back_as_cache_hits() {
    let config = GDeskConfig {
        refine_min_still_frames: 1,
        ..Default::default()
    };
    let mut encoder = GDeskEncoder::new(WIDTH as u16, HEIGHT as u16, config).unwrap();
    let mut decoder = GDeskDecoder::new();
    let mut now_us = 0u64;
    let mut hits = Vec::new();
    for (step, (x, y)) in [(200usize, 120usize), (800, 300), (200, 120)].into_iter().enumerate() {
        let frame = desktop(x, y);
        now_us += 16_000;
        let datagrams = encoder
            .encode_udp_datagrams(&frame, WIDTH * 4, PixelFormat::Rgba, now_us)
            .unwrap();
        let drop_every = (step < 2).then_some(4);
        hits.push(exchange(&mut encoder, &mut decoder, &datagrams, drop_every));
        settle(&mut encoder, &mut decoder, &mut now_us);
        assert_eq!(max_error(decoder.canvas_rgb(), &frame), 0, "step {step}");
    }
    println!("GATE2 cache_hits per step={hits:?}");
    assert_eq!(hits[0], 0);
    assert!(hits[2] > 50, "returning to a confirmed layout must reuse cache slots");
}

#[test]
fn udp_mode_follows_the_qos_traffic_light() {
    use gdesk::QoSState;
    let frame = desktop(200, 120);
    for (state, floor) in [(QoSState::Yellow, (2u8, 4u8)), (QoSState::Red, (4, 6))] {
        let config = GDeskConfig {
            qos_state: state,
            refine_min_still_frames: 1,
            ..Default::default()
        };
        let mut encoder = GDeskEncoder::new(WIDTH as u16, HEIGHT as u16, config).unwrap();
        let datagrams = encoder
            .encode_udp_datagrams(&frame, WIDTH * 4, PixelFormat::Rgba, 1_000)
            .unwrap();
        let mut shifts = Vec::new();
        for datagram in &datagrams {
            for record in gdesk::udp::unpack_udp_datagram(datagram).unwrap().1 {
                if let gdesk::protocol::TileRecord::WaveletInit { shift_y, shift_c, .. } = record {
                    shifts.push((shift_y, shift_c));
                }
            }
        }
        assert!(!shifts.is_empty());
        assert!(shifts.iter().all(|(y, c)| *y >= floor.0 && *c >= floor.1), "{state:?} {shifts:?}");
        for tick in 0..10u64 {
            assert!(encoder.poll_refinement_udp_datagrams(2_000 + tick * 1_000).unwrap().is_empty());
        }
    }
}

#[test]
fn input_packets_respect_the_mtu_and_modifier_guard() {
    let mut tx = InputRedundancyTx::<Vec<u8>>::new(5);
    assert!(tx.push_with_redundancy(0, vec![0u8; 1300], 2).is_err());
    assert!(tx.push_with_redundancy(0, vec![1u8; 100], 2).is_ok());
    let mut rx = InputRedundancyRx::new(InputGuard::default());
    let packet = tx.flush_tail(1).unwrap().remove(0);
    let ctrl_down = |_: &gdesk::udp::InputEvent<Vec<u8>>| Some((gdesk::input::MOD_LCTRL, true));
    assert_eq!(rx.receive_with(&packet, ctrl_down).unwrap().len(), 1);
    tx.push(0, vec![2u8; 4]);
    let release = rx.receive(&tx.encode_input_packet().unwrap()).unwrap();
    assert_eq!(release[0].release, gdesk::input::MOD_LCTRL, "a stuck modifier is released");
}
