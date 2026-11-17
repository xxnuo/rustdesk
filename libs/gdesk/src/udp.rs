use crate::{
    cache::{deterministic_slot, CACHE_CAPACITY},
    decoder::GDeskDecoder,
    encoder::GDeskEncoder,
    input::{InputGuard, InputVerdict},
    math::{self, PixelFormat},
    protocol::{self, GDeskFrameHeader, QoSState, TileRecord, NEW_OS_FRAME},
    Error, Result,
};

pub const GDESK_UDP_MTU: usize = 1200;
// The production transport adds a 14-byte header and a 16-byte AEAD tag.
pub const GDESK_UDP_PAYLOAD_MTU: usize = GDESK_UDP_MTU - 30;
pub const UDP_PACING_WINDOW_US: u64 = 4_000;
const PACE_MIN_BYTES: usize = 64 * 1024;
const PACE_MIN_DATAGRAMS: usize = 48;
const INIT_LADDER: [(u8, u8); 6] = [(0, 0), (2, 4), (4, 6), (6, 8), (8, 10), (15, 15)];

fn datagram_len(header: &GDeskFrameHeader, records: &[TileRecord]) -> Result<usize> {
    Ok(protocol::encode_frame(*header, records)?.len())
}

/// Packs whole tile records greedily into datagrams of at most `GDESK_UDP_MTU` bytes. Every
/// datagram is a regular frame with its own header, so a record is never split.
pub fn pack_udp_datagrams(
    header: &GDeskFrameHeader,
    records: &[TileRecord],
) -> Result<Vec<Vec<u8>>> {
    pack_udp_datagrams_with_mtu(header, records, GDESK_UDP_MTU)
}

pub fn pack_udp_datagrams_with_mtu(
    header: &GDeskFrameHeader,
    records: &[TileRecord],
    mtu: usize,
) -> Result<Vec<Vec<u8>>> {
    if !(64..=GDESK_UDP_MTU).contains(&mtu) { return Err(Error("invalid udp mtu")); }
    let mut out = Vec::new();
    let mut start = 0;
    while start < records.len() {
        let mut end = start + 1;
        if datagram_len(header, &records[start..end])? > mtu {
            return Err(Error("tile record exceeds the udp mtu"));
        }
        while end < records.len() && datagram_len(header, &records[start..end + 1])? <= mtu
        {
            end += 1;
        }
        out.push(protocol::encode_frame(*header, &records[start..end])?);
        start = end;
    }
    Ok(out)
}

pub fn unpack_udp_datagram(datagram: &[u8]) -> Result<(GDeskFrameHeader, Vec<TileRecord>)> {
    if datagram.len() > GDESK_UDP_MTU {
        return Err(Error("udp datagram exceeds the mtu"));
    }
    protocol::decode_frame(datagram)
}

fn record_fits(header: &GDeskFrameHeader, record: &TileRecord, mtu: usize) -> bool {
    datagram_len(header, std::slice::from_ref(record)).is_ok_and(|len| len <= mtu)
}

#[derive(Debug, Clone)]
pub struct UdpEncoderState {
    pub rto_us: u64,
    epoch: Option<u16>,
    sent_seq: Vec<u32>,
    sent_ts_us: Vec<u64>,
    confirmed: Vec<bool>,
    unsent: Vec<bool>,
    pending_parts: Vec<Option<PendingParts>>,
    chain_seq: Vec<u32>,
    sent_slot: Vec<Option<(u16, u64)>>,
    pub unrefinable_tiles: u64,
    pub resent_tiles: u64,
    pub refine_steps: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingParts {
    to: (u8, u8),
    parts: u8,
    next: u8,
}

impl Default for UdpEncoderState {
    fn default() -> Self {
        Self {
            rto_us: 160_000,
            epoch: None,
            sent_seq: Vec::new(),
            sent_ts_us: Vec::new(),
            confirmed: Vec::new(),
            unsent: Vec::new(),
            pending_parts: Vec::new(),
            chain_seq: Vec::new(),
            sent_slot: Vec::new(),
            unrefinable_tiles: 0,
            resent_tiles: 0,
            refine_steps: 0,
        }
    }
}

impl UdpEncoderState {
    fn ensure(&mut self, epoch: u16, count: usize) -> bool {
        if self.epoch != Some(epoch) || self.sent_seq.len() != count {
            self.epoch = Some(epoch);
            self.sent_seq = vec![0; count];
            self.sent_ts_us = vec![0; count];
            self.confirmed = vec![false; count];
            self.unsent = vec![false; count];
            self.pending_parts = vec![None; count];
            self.chain_seq = vec![0; count];
            self.sent_slot = vec![None; count];
            return true;
        }
        false
    }

    pub fn unconfirmed_tiles(&self) -> usize {
        self.sent_seq
            .iter()
            .zip(&self.confirmed)
            .filter(|(seq, confirmed)| **seq != 0 && !**confirmed)
            .count()
    }
}

impl GDeskEncoder {
    fn udp_header(&self, frame_seq: u32, capture_ts_us: u64) -> GDeskFrameHeader {
        GDeskFrameHeader {
            width: self.width,
            height: self.height,
            canvas_epoch: self.canvas_epoch,
            frame_seq,
            frame_flags: NEW_OS_FRAME,
            qos_state: self.config.qos_state,
            capture_ts_us,
            ..Default::default()
        }
    }

    fn qos_floor(&self) -> (u8, u8) {
        let qos = match self.config.qos_state {
            QoSState::Green => (0, 0),
            QoSState::Yellow => (2, 4),
            QoSState::Red => (4, 6),
        };
        let initial = self.config.initial_shifts.unwrap_or((0, 0));
        (qos.0.max(initial.0), qos.1.max(initial.1))
    }

    fn insert_lossless(&mut self, index: usize, rgb: &[u8]) -> Result<()> {
        let hash = math::tile_hash(rgb);
        let slot = deterministic_slot(hash);
        self.cache.insert_at_slot(slot, rgb)?;
        self.udp.sent_slot[index] = Some((slot, hash));
        Ok(())
    }

    fn encode_tile_udp(
        &mut self,
        index: usize,
        header: &GDeskFrameHeader,
        capture_ts_us: u64,
        allow_hit: bool,
    ) -> Result<TileRecord> {
        let tile_index = index as u16;
        let tile = self.states.tiles[index].last_rgb.clone();
        self.udp.sent_slot[index] = None;
        let hit = if allow_hit {
            self.cache.find(math::tile_hash(&tile), &tile, true)
        } else {
            None
        };
        let record = if let Some(slot_id) = hit {
            let state = &mut self.states.tiles[index];
            state.sent_shift_y = 0;
            state.sent_shift_c = 0;
            state.coefficients = None;
            self.stats.cache_hits += 1;
            TileRecord::CacheHit {
                tile_index,
                slot_id,
            }
        } else if let Some(color) = math::detect_solid(&tile) {
            let state = &mut self.states.tiles[index];
            state.sent_shift_y = 0;
            state.sent_shift_c = 0;
            state.coefficients = None;
            TileRecord::SolidFill { tile_index, color }
        } else if let Some((colors, mask)) = math::detect_two_color(&tile) {
            self.insert_lossless(index, &tile)?;
            let state = &mut self.states.tiles[index];
            state.sent_shift_y = 0;
            state.sent_shift_c = 0;
            state.coefficients = None;
            TileRecord::TwoColorText {
                tile_index,
                colors,
                mask,
            }
        } else {
            let coeff = Self::coefficients(
                #[cfg(feature = "gpu")] &mut self.gpu,
                index, &tile,
            )?;
            let mut chosen = None;
            let floor = self.qos_floor();
            let mut ladder = vec![floor];
            ladder.extend(
                INIT_LADDER
                    .iter()
                    .copied()
                    .filter(|(y, c)| *y >= floor.0 && *c >= floor.1 && (*y, *c) != floor),
            );
            for (shift_y, shift_c) in ladder {
                let record = TileRecord::WaveletInit {
                    tile_index,
                    shift_y,
                    shift_c,
                    payload: math::encode_wavelet_init(&coeff, shift_y, shift_c)?,
                };
                if record_fits(header, &record, self.config.udp_mtu) {
                    chosen = Some(record);
                    break;
                }
            }
            let record = chosen.ok_or(Error("tile record exceeds the udp mtu"))?;
            let (shift_y, shift_c) = match &record {
                TileRecord::WaveletInit {
                    shift_y, shift_c, ..
                } => (*shift_y, *shift_c),
                _ => (0, 0),
            };
            if shift_y == 0 && shift_c == 0 {
                self.insert_lossless(index, &tile)?;
            }
            let state = &mut self.states.tiles[index];
            state.sent_shift_y = shift_y;
            state.sent_shift_c = shift_c;
            state.coefficients = (shift_y != 0 || shift_c != 0).then_some(coeff);
            record
        };
        self.udp.sent_seq[index] = header.frame_seq;
        self.udp.sent_ts_us[index] = capture_ts_us;
        self.udp.confirmed[index] = false;
        self.udp.unsent[index] = false;
        self.udp.pending_parts[index] = None;
        self.udp.chain_seq[index] = header.frame_seq;
        Ok(record)
    }

    fn refine_tile_udp(
        &mut self,
        index: usize,
        header: &GDeskFrameHeader,
        capture_ts_us: u64,
    ) -> Result<Option<TileRecord>> {
        let state = &self.states.tiles[index];
        let coeff = state
            .coefficients
            .as_ref()
            .ok_or(Error("missing refinement coefficients"))?;
        let old = (state.sent_shift_y, state.sent_shift_c);
        let chain = self.udp.chain_seq[index];
        let make = |new: (u8, u8), part: usize, parts: usize| -> Result<TileRecord> {
            let mut payload = vec![old.0 << 4 | old.1, (part as u8) << 4 | parts as u8];
            payload.extend_from_slice(&chain.to_le_bytes());
            payload.extend(if parts == 1 {
                math::encode_wavelet_refine(coeff, old.0, old.1, new.0, new.1)?
            } else {
                math::encode_wavelet_refine_part(coeff, old, new, part, parts)?
            });
            Ok(TileRecord::WaveletRefine {
                tile_index: index as u16,
                shift_y: new.0,
                shift_c: new.1,
                payload,
            })
        };
        let mut plan = self.udp.pending_parts[index].map(|p| (p.to, p.parts as usize, p.next as usize));
        if plan.is_none() {
            let mut candidates: Vec<(u8, u8)> = (0..=old.0)
                .flat_map(|y| (0..=old.1).map(move |c| (y, c)))
                .filter(|new| *new != old)
                .collect();
            candidates.sort_by_key(|(y, c)| y + c);
            for new in candidates {
                if record_fits(header, &make(new, 0, 1)?, self.config.udp_mtu) {
                    plan = Some((new, 1, 0));
                    break;
                }
            }
        }
        if plan.is_none() {
            let new = if old.1 > 0 {
                (old.0, old.1 - 1)
            } else {
                (old.0.saturating_sub(1), old.1)
            };
            'parts: for parts in [2usize, 4, 8] {
                for part in 0..parts {
                    if !record_fits(header, &make(new, part, parts)?, self.config.udp_mtu) {
                        continue 'parts;
                    }
                }
                plan = Some((new, parts, 0));
                break;
            }
        }
        let Some((new, parts, part)) = plan else {
            self.udp.unrefinable_tiles += 1;
            self.states.tiles[index].still_frames = 0;
            return Ok(None);
        };
        self.udp.refine_steps += 1;
        let record = make(new, part, parts)?;
        let finished = part + 1 == parts;
        let lossless_rgb = if finished && new == (0, 0) {
            Some(math::wavelet_to_rgb(coeff)?)
        } else {
            None
        };
        if let Some(rgb) = &lossless_rgb {
            self.insert_lossless(index, rgb)?;
        }
        let state = &mut self.states.tiles[index];
        if finished {
            state.sent_shift_y = new.0;
            state.sent_shift_c = new.1;
            if lossless_rgb.is_some() {
                state.coefficients = None;
            }
            self.udp.pending_parts[index] = None;
        } else {
            self.udp.pending_parts[index] = Some(PendingParts {
                to: new,
                parts: parts as u8,
                next: part as u8 + 1,
            });
        }
        self.udp.sent_seq[index] = header.frame_seq;
        self.udp.sent_ts_us[index] = capture_ts_us;
        self.udp.confirmed[index] = false;
        Ok(Some(record))
    }

    fn finish_udp(
        &mut self,
        mut header: GDeskFrameHeader,
        records: Vec<TileRecord>,
        rounds: usize,
        start: std::time::Instant,
    ) -> Result<Vec<Vec<u8>>> {
        header.encode_send_delay_us = start.elapsed().as_micros().min(u32::MAX as u128) as u32;
        let unique = pack_udp_datagrams_with_mtu(&header, &records, self.config.udp_mtu)?;
        let mut datagrams = Vec::with_capacity(unique.len() * rounds);
        for _ in 0..rounds.max(1) {
            datagrams.extend(unique.iter().cloned());
        }
        let sent_at = header.capture_ts_us.saturating_add(start.elapsed().as_micros().min(u64::MAX as u128) as u64);
        for record in &records {
            self.udp.sent_ts_us[record.tile_index() as usize] = sent_at;
        }
        self.stats.frames += 1;
        self.stats.encoded_bytes += datagrams.iter().map(|d| d.len() as u64).sum::<u64>();
        Ok(datagrams)
    }

    /// Encodes changed tiles (and tiles the peer has not confirmed) into MTU-sized datagrams.
    pub fn encode_udp_datagrams(
        &mut self,
        pixels: &[u8],
        stride: usize,
        format: PixelFormat,
        capture_ts_us: u64,
    ) -> Result<Vec<Vec<u8>>> {
        self.config.validate()?;
        math::validate_image(
            pixels,
            self.width as usize,
            self.height as usize,
            stride,
            format,
        )?;
        let start = std::time::Instant::now();
        #[cfg(feature = "gpu")]
        self.prepare_gpu(pixels, stride, format, true)?;
        let count = self.states.tiles.len();
        if self.udp.ensure(self.canvas_epoch, count) {
            self.cache.confirmed_slots.fill(false);
        }
        self.last_capture_ts_us = capture_ts_us;
        let header = self.udp_header(self.frame_seq.wrapping_add(1), capture_ts_us);
        let mut records = Vec::new();
        for index in 0..count {
            let tile = math::extract_tile(
                pixels,
                self.width as usize,
                self.height as usize,
                stride,
                format,
                index,
            )?;
            let hash = math::tile_hash(&tile);
            let unsent = self.udp.unsent[index];
            let state = &mut self.states.tiles[index];
            if state.initialized && !unsent && state.last_hash == hash && state.last_rgb == tile {
                state.still_frames = state.still_frames.saturating_add(1);
                continue;
            }
            state.last_rgb = tile;
            state.last_hash = hash;
            state.still_frames = 0;
            state.initialized = true;
            records.push(self.encode_tile_udp(index, &header, capture_ts_us, !unsent)?);
            self.stats.changed_tiles += 1;
        }
        if records.is_empty() {
            return Ok(Vec::new());
        }
        self.frame_seq = header.frame_seq;
        self.finish_udp(
            header,
            records,
            self.config.udp_redundancy_factor as usize,
            start,
        )
    }

    /// Resends tiles whose ack timed out or reported a loss, then advances refinement one step.
    pub fn poll_refinement_udp_datagrams(&mut self, capture_ts_us: u64) -> Result<Vec<Vec<u8>>> {
        self.config.validate()?;
        if self.config.qos_state != QoSState::Green {
            return Ok(Vec::new());
        }
        let start = std::time::Instant::now();
        let count = self.states.tiles.len();
        if self.udp.ensure(self.canvas_epoch, count) {
            self.cache.confirmed_slots.fill(false);
        }
        self.last_capture_ts_us = capture_ts_us;
        let rto = self.udp.rto_us;
        for index in 0..count {
            let udp = &mut self.udp;
            if udp.sent_seq[index] != 0
                && !udp.confirmed[index]
                && capture_ts_us.saturating_sub(udp.sent_ts_us[index]) > rto
            {
                udp.unsent[index] = true;
            }
            let state = &mut self.states.tiles[index];
            if state.needs_refinement() {
                state.still_frames = state.still_frames.saturating_add(1);
            }
        }
        let header = self.udp_header(self.frame_seq.wrapping_add(1), capture_ts_us);
        let mut records = Vec::new();
        let mut refined = 0;
        for index in 0..count {
            let state = &self.states.tiles[index];
            if self.udp.unsent[index] && state.initialized {
                self.udp.resent_tiles += 1;
                records.push(self.encode_tile_udp(index, &header, capture_ts_us, false)?);
            } else if state.needs_refinement()
                && state.still_frames >= self.config.refine_min_still_frames
                && refined < self.config.max_refine_tiles_per_frame
            {
                if let Some(record) = self.refine_tile_udp(index, &header, capture_ts_us)? {
                    records.push(record);
                    refined += 1;
                }
            }
        }
        self.stats.refined_tiles += refined as u64;
        if records.is_empty() {
            return Ok(Vec::new());
        }
        self.frame_seq = header.frame_seq;
        self.finish_udp(
            header,
            records,
            self.config.udp_refine_redundancy_factor as usize,
            start,
        )
    }

    /// `bitmap` has one bit per tile and reports the tiles the peer applied from frame
    /// `acked_frame_seq`. Confirmed lossless tiles confirm their cache slot; tiles of that frame
    /// missing from the bitmap are queued for retransmission.
    pub fn apply_tile_ack_bitmap(&mut self, acked_frame_seq: u32, bitmap: &[u8]) -> Result<()> {
        let count = self.states.tiles.len();
        if bitmap.len() != count.div_ceil(8) {
            return Err(Error("invalid tile ack bitmap"));
        }
        if self.udp.ensure(self.canvas_epoch, count) {
            self.cache.confirmed_slots.fill(false);
        }
        for index in 0..count {
            if self.udp.sent_seq[index] != acked_frame_seq || self.udp.confirmed[index] {
                continue;
            }
            if bitmap[index / 8] >> (index % 8) & 1 == 1 {
                self.udp.confirmed[index] = true;
                self.udp.unsent[index] = false;
                if self.states.tiles[index].is_lossless() {
                    if let Some((slot, hash)) = self.udp.sent_slot[index] {
                        if self.cache.get(slot).is_some_and(|entry| entry.hash == hash) {
                            self.cache.confirm_slot(slot)?;
                        }
                    }
                }
            } else {
                self.udp.unsent[index] = true;
            }
        }
        Ok(())
    }

    pub fn has_udp_unconfirmed(&self) -> bool {
        self.udp.unconfirmed_tiles() > 0 || self.has_pending_refinement()
    }
}

#[derive(Debug, Default)]
pub struct UdpDecoderState {
    epoch: u16,
    pub(crate) active: bool,
    locked: bool,
    tile_seq: Vec<u32>,
    chain: Vec<u32>,
    slot_ver: Vec<(u32, u16)>,
    staged: std::collections::HashMap<usize, StagedRefine>,
    stash: std::collections::HashMap<usize, Vec<StashedRefine>>,
}

impl UdpDecoderState {
    /// Marks the session as reliable: standard cache insertion applies and UDP datagrams of this
    /// or an older epoch are refused.
    pub(crate) fn reliable(epoch: u16) -> Self {
        Self {
            epoch,
            locked: true,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
struct StashedRefine {
    seq: u32,
    to: (u8, u8),
    payload: Vec<u8>,
}

enum RefineOutcome {
    Applied,
    NotYet,
    Reject,
    Invalid,
}

const MAX_STASHED_REFINES: usize = 32;

fn chain_of(payload: &[u8]) -> Option<u32> {
    payload
        .get(2..6)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[derive(Debug)]
struct StagedRefine {
    from: u8,
    to: (u8, u8),
    parts: usize,
    next: usize,
    state: math::WaveletState,
}

impl GDeskDecoder {
    /// Applies one self-contained datagram. Loss and reordering are tolerated: a tile record is
    /// applied only when it is newer than what the tile already shows, and a refinement only when
    /// the tile is at the quality the refinement was computed from. A malformed record is an error.
    pub fn decode_udp_datagram(&mut self, datagram: &[u8]) -> Result<Vec<u16>> {
        let (header, records) = unpack_udp_datagram(datagram)?;
        let count = protocol::tile_count(header.width, header.height)?;
        let same_canvas = self.udp.active
            && self.udp.epoch == header.canvas_epoch
            && self.udp.tile_seq.len() == count
            && self
                .last_header
                .is_some_and(|h| (h.width, h.height) == (header.width, header.height));
        if !same_canvas {
            if (self.udp.active || self.udp.locked)
                && (header.canvas_epoch.wrapping_sub(self.udp.epoch) as i16) <= 0
            {
                return Err(Error("stale canvas epoch"));
            }
            self.switch_udp_canvas(&header, count);
        }
        let mut updated = Vec::new();
        for record in records {
            let index = record.tile_index() as usize;
            let seen = self.udp.tile_seq[index];
            if seen != 0 && (header.frame_seq.wrapping_sub(seen) as i32) <= 0 {
                continue;
            }
            if let TileRecord::WaveletRefine {
                shift_y,
                shift_c,
                payload,
                ..
            } = record
            {
                let to = (shift_y, shift_c);
                match self.apply_udp_refine(&header, index, to, &payload) {
                    RefineOutcome::Applied => {
                        if self.lossless[index] {
                            self.udp_cache_insert(header.frame_seq, index);
                        }
                        self.udp.tile_seq[index] = header.frame_seq;
                        updated.push(index as u16);
                        self.drain_stash(&header, index, &mut updated);
                    }
                    RefineOutcome::NotYet => {
                        let stash = self.udp.stash.entry(index).or_default();
                        if stash.len() >= MAX_STASHED_REFINES {
                            stash.remove(0);
                        }
                        stash.push(StashedRefine {
                            seq: header.frame_seq,
                            to,
                            payload,
                        });
                    }
                    RefineOutcome::Reject => {}
                    RefineOutcome::Invalid => return Err(Error("invalid refinement record")),
                }
                continue;
            }
            let mut caches_tile = false;
            match &record {
                TileRecord::CacheHit { slot_id, .. } => {
                    let version = (header.frame_seq, index as u16);
                    let current = self.udp.slot_ver.get(*slot_id as usize).copied();
                    if self.cache.get(*slot_id).is_none() || current.is_none_or(|ver| ver > version) {
                        continue;
                    }
                }
                TileRecord::TwoColorText { .. } => caches_tile = true,
                TileRecord::WaveletInit {
                    shift_y, shift_c, ..
                } => caches_tile = *shift_y == 0 && *shift_c == 0,
                _ => {}
            }
            self.udp.staged.remove(&index);
            self.apply_record(&header, record)?;
            self.udp.chain[index] = header.frame_seq;
            self.udp.tile_seq[index] = header.frame_seq;
            updated.push(index as u16);
            if caches_tile {
                self.udp_cache_insert(header.frame_seq, index);
            }
            if let Some(stash) = self.udp.stash.get_mut(&index) {
                stash.retain(|entry| {
                    chain_of(&entry.payload)
                        .is_some_and(|chain| (chain.wrapping_sub(header.frame_seq) as i32) >= 0)
                });
            }
            self.drain_stash(&header, index, &mut updated);
        }
        if !updated.is_empty() && !self.is_lossless() {
            self.render();
        }
        Ok(updated)
    }

    /// A new epoch keeps what is on screen: same size stays in place, a new size keeps the
    /// overlapping pixels. Every tile still has to be received again.
    fn switch_udp_canvas(&mut self, header: &GDeskFrameHeader, count: usize) {
        let (width, height) = (header.width as usize, header.height as usize);
        let mut canvas = vec![0u8; width * height * 3];
        if let Some(old) = self.last_header.filter(|_| !self.canvas.is_empty()) {
            let (old_width, old_height) = (old.width as usize, old.height as usize);
            let copy_width = width.min(old_width) * 3;
            for y in 0..height.min(old_height) {
                canvas[y * width * 3..y * width * 3 + copy_width]
                    .copy_from_slice(&self.canvas[y * old_width * 3..y * old_width * 3 + copy_width]);
            }
        }
        self.rendered_canvas = canvas.clone();
        self.canvas = canvas;
        self.wavelets = vec![None; count];
        self.lossless = vec![false; count];
        self.needs_reset = false;
        self.cache.clear();
        self.udp = UdpDecoderState {
            epoch: header.canvas_epoch,
            active: true,
            locked: false,
            tile_seq: vec![0; count],
            chain: vec![0; count],
            slot_ver: vec![(0, 0); CACHE_CAPACITY],
            staged: Default::default(),
            stash: Default::default(),
        };
        self.last_header = Some(*header);
    }

    fn udp_cache_insert(&mut self, seq: u32, index: usize) {
        let Some(header) = self.last_header else {
            return;
        };
        let (width, height) = (header.width as usize, header.height as usize);
        let Ok(rgb) = math::extract_tile(
            &self.canvas,
            width,
            height,
            width * 3,
            PixelFormat::Rgb,
            index,
        ) else {
            return;
        };
        let slot = deterministic_slot(math::tile_hash(&rgb));
        let version = (seq, index as u16);
        if self.udp.slot_ver[slot as usize] < version
            && self.cache.insert_at_slot(slot, &rgb).is_ok()
        {
            self.udp.slot_ver[slot as usize] = version;
        }
    }

    fn drain_stash(&mut self, header: &GDeskFrameHeader, index: usize, updated: &mut Vec<u16>) {
        loop {
            let Some(mut entries) = self.udp.stash.remove(&index) else {
                return;
            };
            entries.sort_by_key(|entry| entry.seq);
            let mut progressed = false;
            let mut kept = Vec::new();
            for entry in entries {
                if progressed {
                    kept.push(entry);
                    continue;
                }
                let entry_header = GDeskFrameHeader {
                    frame_seq: entry.seq,
                    ..*header
                };
                match self.apply_udp_refine(&entry_header, index, entry.to, &entry.payload) {
                    RefineOutcome::Applied => {
                        if self.lossless[index] {
                            self.udp_cache_insert(entry.seq, index);
                        }
                        self.udp.tile_seq[index] = self.udp.tile_seq[index].max(entry.seq);
                        updated.push(index as u16);
                        progressed = true;
                    }
                    RefineOutcome::NotYet => kept.push(entry),
                    RefineOutcome::Reject | RefineOutcome::Invalid => {}
                }
            }
            if !kept.is_empty() {
                self.udp.stash.insert(index, kept);
            }
            if !progressed {
                return;
            }
        }
    }

    fn apply_udp_refine(
        &mut self,
        header: &GDeskFrameHeader,
        index: usize,
        to: (u8, u8),
        payload: &[u8],
    ) -> RefineOutcome {
        let (Some(&from), Some(&split), Some(chain)) =
            (payload.first(), payload.get(1), chain_of(payload))
        else {
            return RefineOutcome::Invalid;
        };
        let (part, parts) = ((split >> 4) as usize, (split & 15) as usize);
        if parts == 0 || part >= parts {
            return RefineOutcome::Invalid;
        }
        let seen_chain = self.udp.chain[index];
        let lag = chain.wrapping_sub(seen_chain) as i32;
        if lag < 0 {
            return RefineOutcome::Reject;
        }
        let body = &payload[6..];
        let Some(current) = self.wavelets[index].as_ref() else {
            return if lag > 0 || self.udp.tile_seq[index] == 0 {
                RefineOutcome::NotYet
            } else {
                RefineOutcome::Reject
            };
        };
        if lag > 0 {
            return RefineOutcome::NotYet;
        }
        let have = (current.shift_y, current.shift_c);
        let want = (from >> 4, from & 15);
        if have != want {
            return if have.0 + have.1 > want.0 + want.1 {
                RefineOutcome::NotYet
            } else {
                RefineOutcome::Reject
            };
        }
        if parts == 1 {
            return if self
                .apply_record(
                    header,
                    TileRecord::WaveletRefine {
                        tile_index: index as u16,
                        shift_y: to.0,
                        shift_c: to.1,
                        payload: body.to_vec(),
                    },
                )
                .is_ok()
            {
                RefineOutcome::Applied
            } else {
                RefineOutcome::Invalid
            };
        }
        let mut staged = if part == 0 {
            StagedRefine {
                from,
                to,
                parts,
                next: 0,
                state: current.clone(),
            }
        } else {
            match self.udp.staged.remove(&index) {
                Some(staged)
                    if staged.from == from
                        && staged.to == to
                        && staged.parts == parts
                        && staged.next == part =>
                {
                    staged
                }
                Some(staged) => {
                    self.udp.staged.insert(index, staged);
                    return RefineOutcome::NotYet;
                }
                None => return RefineOutcome::NotYet,
            }
        };
        if math::decode_wavelet_refine_part(&mut staged.state, body, to, part, parts).is_err() {
            return RefineOutcome::Invalid;
        }
        staged.next += 1;
        if staged.next < parts {
            self.udp.staged.insert(index, staged);
            self.stats.wavelet_refines = self.stats.wavelet_refines.saturating_add(1);
            return RefineOutcome::Applied;
        }
        staged.state.shift_y = to.0;
        staged.state.shift_c = to.1;
        let Ok(rgb) = staged.state.reconstruct() else {
            return RefineOutcome::Invalid;
        };
        let lossless = to == (0, 0);
        self.wavelets[index] = (!lossless).then_some(staged.state);
        self.lossless[index] = lossless;
        if math::write_tile(
            &rgb,
            &mut self.canvas,
            header.width as usize,
            header.height as usize,
            header.width as usize * 3,
            PixelFormat::Rgb,
            index,
        )
        .is_ok()
        {
            self.stats.wavelet_refines = self.stats.wavelet_refines.saturating_add(1);
            RefineOutcome::Applied
        } else {
            RefineOutcome::Invalid
        }
    }

    pub fn udp_tile_received(&self, index: usize) -> bool {
        self.udp.tile_seq.get(index).is_some_and(|seq| *seq != 0)
    }

    /// Bit `n` is set when tile `n` was last updated by frame `frame_seq`.
    pub fn tile_ack_bitmap(&self, frame_seq: u32) -> Vec<u8> {
        let mut bitmap = vec![0u8; self.udp.tile_seq.len().div_ceil(8)];
        for (index, seq) in self.udp.tile_seq.iter().enumerate() {
            if *seq == frame_seq {
                bitmap[index / 8] |= 1 << (index % 8);
            }
        }
        bitmap
    }
}

/// Spreads the datagrams of a burst over `UDP_PACING_WINDOW_US`; small frames go out at once.
pub fn pacing_offsets_us(datagram_lens: &[usize]) -> Vec<u64> {
    let total: usize = datagram_lens.iter().sum();
    if total <= PACE_MIN_BYTES && datagram_lens.len() <= PACE_MIN_DATAGRAMS {
        return vec![0; datagram_lens.len()];
    }
    let mut sent = 0usize;
    datagram_lens
        .iter()
        .map(|len| {
            let offset = UDP_PACING_WINDOW_US * sent as u64 / total as u64;
            sent += len;
            offset
        })
        .collect()
}

const VIRTUAL_NIC_MARKERS: [&str; 12] = [
    "docker",
    "wsl",
    "vethernet",
    "zerotier",
    "tailscale",
    "utun",
    "tun",
    "tap",
    "virbr",
    "veth",
    "vmnet",
    "vboxnet",
];

pub fn is_virtual_nic(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    VIRTUAL_NIC_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
}

/// Stable: physical interfaces first, virtual ones after, relative order preserved.
pub fn sort_candidate_interfaces<T, F: Fn(&T) -> &str>(candidates: &mut [T], name_fn: F) {
    candidates.sort_by_key(|candidate| is_virtual_nic(name_fn(candidate)));
}

pub const INPUT_RING_DEPTH: usize = 4;
const INPUT_MAGIC: u8 = 0xD1;

pub trait InputPayload {
    fn to_payload(&self) -> Vec<u8>;
}
impl InputPayload for Vec<u8> {
    fn to_payload(&self) -> Vec<u8> {
        self.clone()
    }
}
impl InputPayload for u32 {
    fn to_payload(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}
impl InputPayload for u64 {
    fn to_payload(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent<T> {
    pub seq: u32,
    pub lease_id: u32,
    pub modifier_state_mask: u16,
    pub payload: T,
}

/// Keeps the newest four events and sends all of them in every packet, so up to three
/// consecutive lost packets lose nothing.
#[derive(Debug, Clone)]
pub struct InputRedundancyTx<T> {
    ring: Vec<InputEvent<T>>,
    next_seq: u32,
    lease_id: u32,
}

impl<T: InputPayload + Clone> InputRedundancyTx<T> {
    pub fn new(lease_id: u32) -> Self {
        Self {
            ring: Vec::with_capacity(INPUT_RING_DEPTH),
            next_seq: 1,
            lease_id,
        }
    }

    pub fn push(&mut self, modifier_state_mask: u16, payload: T) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.ring.len() == INPUT_RING_DEPTH {
            self.ring.remove(0);
        }
        self.ring.push(InputEvent {
            seq,
            lease_id: self.lease_id,
            modifier_state_mask,
            payload,
        });
        seq
    }

    /// Pushes the event and returns `copies` identical packets for the caller to space out in
    /// time; every copy carries the newest four events.
    pub fn push_with_redundancy(
        &mut self,
        modifier_state_mask: u16,
        payload: T,
        copies: usize,
    ) -> Result<Vec<Vec<u8>>> {
        self.push(modifier_state_mask, payload);
        self.flush_tail(copies).map_err(|error| {
            self.ring.pop();
            self.next_seq = self.next_seq.wrapping_sub(1);
            error
        })
    }

    /// Keepalive packets for the idle tail, so the last events survive a burst of losses.
    pub fn flush_tail(&self, copies: usize) -> Result<Vec<Vec<u8>>> {
        let packet = self.encode_input_packet()?;
        Ok(vec![packet; copies])
    }

    /// Newest event first. Calling it again without `push` makes a keepalive resend.
    pub fn encode_input_packet(&self) -> Result<Vec<u8>> {
        let mut out = vec![INPUT_MAGIC, self.ring.len() as u8];
        for event in self.ring.iter().rev() {
            let payload = event.payload.to_payload();
            out.extend_from_slice(&event.seq.to_le_bytes());
            out.extend_from_slice(&event.lease_id.to_le_bytes());
            out.extend_from_slice(&event.modifier_state_mask.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            out.extend_from_slice(&payload);
        }
        if out.len() > GDESK_UDP_MTU {
            return Err(Error("input packet exceeds the udp mtu"));
        }
        Ok(out)
    }
}

pub fn decode_input_packet(buf: &[u8]) -> Result<Vec<InputEvent<Vec<u8>>>> {
    if buf.len() < 2 || buf[0] != INPUT_MAGIC || buf[1] as usize > INPUT_RING_DEPTH {
        return Err(Error("invalid input packet"));
    }
    let mut at = 2;
    let mut events = Vec::new();
    for _ in 0..buf[1] {
        let head = buf.get(at..at + 12).ok_or(Error("truncated input packet"))?;
        let len = u16::from_le_bytes([head[10], head[11]]) as usize;
        let payload = buf
            .get(at + 12..at + 12 + len)
            .ok_or(Error("truncated input packet"))?;
        events.push(InputEvent {
            seq: u32::from_le_bytes([head[0], head[1], head[2], head[3]]),
            lease_id: u32::from_le_bytes([head[4], head[5], head[6], head[7]]),
            modifier_state_mask: u16::from_le_bytes([head[8], head[9]]),
            payload: payload.to_vec(),
        });
        at += 12 + len;
    }
    if at != buf.len() {
        return Err(Error("trailing input data"));
    }
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedInput {
    pub seq: u32,
    pub payload: Vec<u8>,
    pub release: u16,
}

#[derive(Debug, Default)]
pub struct InputRedundancyRx {
    guard: InputGuard,
    last_executed: u32,
    pub executed: u64,
    pub lost: u64,
    pub rejected: u64,
    pub suppressed: u64,
}

impl InputRedundancyRx {
    pub fn new(guard: InputGuard) -> Self {
        Self {
            guard,
            ..Default::default()
        }
    }

    /// Executes the events of one packet that have not run yet, oldest first.
    pub fn receive(&mut self, buf: &[u8]) -> Result<Vec<ExecutedInput>> {
        self.receive_with(buf, |_| None)
    }

    /// `modifier_event` names the modifier bit and direction an event presses or releases, so the
    /// guard can tell a stuck modifier from a held one.
    pub fn receive_with<F: FnMut(&InputEvent<Vec<u8>>) -> Option<(u16, bool)>>(
        &mut self,
        buf: &[u8],
        mut modifier_event: F,
    ) -> Result<Vec<ExecutedInput>> {
        let mut events = decode_input_packet(buf)?;
        events.sort_by_key(|event| event.seq.wrapping_sub(self.last_executed));
        let mut out = Vec::new();
        for event in events {
            if (event.seq.wrapping_sub(self.last_executed) as i32) <= 0 {
                self.suppressed += 1;
                continue;
            }
            let modifier = modifier_event(&event);
            let release = match self
                .guard
                .check(event.lease_id, event.modifier_state_mask, modifier)
            {
                InputVerdict::Reject => {
                    self.rejected += 1;
                    continue;
                }
                InputVerdict::Accept { release } => release,
            };
            let gap = event.seq.wrapping_sub(self.last_executed).wrapping_sub(1);
            self.lost += gap as u64;
            self.last_executed = event.seq;
            self.executed += 1;
            out.push(ExecutedInput {
                seq: event.seq,
                payload: event.payload,
                release,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_retransmission_clock_starts_after_encoding() {
        let mut encoder = GDeskEncoder::new(64, 64, crate::GDeskConfig::default()).unwrap();
        let rgb: Vec<u8> = (0..64 * 64 * 3).map(|i| (i * 37 + i / 17) as u8).collect();
        let packets = encoder.encode_udp_datagrams(&rgb, 64 * 3, PixelFormat::Rgb, 10).unwrap();
        let header = GDeskFrameHeader::from_bytes(&packets[0]).unwrap();
        assert!(header.encode_send_delay_us > 1);
        encoder.udp.rto_us = 1;
        let next = encoder.poll_refinement_udp_datagrams(10 + header.encode_send_delay_us as u64).unwrap();
        for packet in next {
            let (_, records) = unpack_udp_datagram(&packet).unwrap();
            assert!(records.iter().all(|r| matches!(r, TileRecord::WaveletRefine { .. })));
        }
    }

    #[test]
    fn payload_budget_keeps_initial_and_split_refinement_within_aead_mtu() {
        let mut encoder = GDeskEncoder::new(512, 64, crate::GDeskConfig {
            udp_mtu: GDESK_UDP_PAYLOAD_MTU, ..Default::default()
        }).unwrap();
        let mut seed = 17u32;
        let rgb: Vec<u8> = (0..512 * 64 * 3).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        }).collect();
        let mut decoder = GDeskDecoder::new();
        let mut packets = encoder.encode_udp_datagrams(&rgb, 512 * 3, PixelFormat::Rgb, 1).unwrap();
        let mut split = false;
        assert!(packets.len() > 1);
        for now in 2..500 {
            for packet in &packets {
                assert!(packet.len() + 30 <= GDESK_UDP_MTU);
                let (header, records) = unpack_udp_datagram(packet).unwrap();
                let last_seq = header.frame_seq;
                split |= records.iter().any(|r| matches!(r,
                    TileRecord::WaveletRefine { payload, .. } if payload[1] & 15 > 1));
                decoder.decode_udp_datagram(packet).unwrap();
                encoder.apply_tile_ack_bitmap(last_seq, &decoder.tile_ack_bitmap(last_seq)).unwrap();
            }
            if now == 2 {
                assert!(encoder.encode_udp_datagrams(&rgb, 512 * 3, PixelFormat::Rgb, now).unwrap().is_empty());
            }
            if !encoder.has_udp_unconfirmed() { break; }
            packets = encoder.poll_refinement_udp_datagrams(now).unwrap();
        }
        assert!(split, "fixture must exercise split refinement records");
        assert!(!encoder.has_udp_unconfirmed());
        assert_eq!(decoder.canvas_rgb(), rgb);
        assert!(encoder.poll_refinement_udp_datagrams(501).unwrap().is_empty());
    }

    #[test]
    fn nic_sort_is_stable_and_demotes_virtual() {
        let mut names = vec!["docker0", "eth0", "vEthernet (WSL)", "wlan0", "tun0", "Ethernet"];
        sort_candidate_interfaces(&mut names, |name| name);
        assert_eq!(
            names,
            ["eth0", "wlan0", "Ethernet", "docker0", "vEthernet (WSL)", "tun0"]
        );
    }

    #[test]
    fn input_ring_survives_three_lost_packets() {
        let mut tx = InputRedundancyTx::<u32>::new(7);
        let mut rx = InputRedundancyRx::new(InputGuard::default());
        let mut packets = Vec::new();
        for key in 0..8u32 {
            tx.push(0, key);
            packets.push(tx.encode_input_packet().unwrap());
        }
        let mut seen = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            if (2..5).contains(&index) {
                continue;
            }
            for event in rx.receive(packet).unwrap() {
                seen.push(event.seq);
            }
        }
        assert_eq!(seen, (1..=8).collect::<Vec<_>>());
        assert_eq!((rx.lost, rx.rejected), (0, 0));
    }
}
