use crate::{Error, Result};

pub const MAGIC_VERSION: u16 = 0x5044;
pub const HEADER_LEN: usize = 32;
pub const NEW_OS_FRAME: u8 = 1;
pub const KEYFRAME_RESET: u8 = 2;
pub const LZ4_COMPRESSED: u8 = 4;
pub const MAX_TILES: usize = 8192;
pub const MAX_UNCOMPRESSED_LEN: usize = MAX_TILES * 27000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum QoSState {
    #[default]
    Green = 0,
    Yellow = 1,
    Red = 2,
}
impl TryFrom<u8> for QoSState {
    type Error = Error;
    fn try_from(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Green),
            1 => Ok(Self::Yellow),
            2 => Ok(Self::Red),
            _ => Err(Error("invalid QoS state")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GDeskFrameHeader {
    pub magic_version: u16,
    pub canvas_epoch: u16,
    pub width: u16,
    pub height: u16,
    pub frame_seq: u32,
    pub frame_flags: u8,
    pub qos_state: QoSState,
    pub tile_record_count: u16,
    pub capture_ts_us: u64,
    pub encode_send_delay_us: u32,
    pub uncompressed_len: u32,
}
impl Default for GDeskFrameHeader {
    fn default() -> Self {
        Self {
            magic_version: MAGIC_VERSION,
            canvas_epoch: 0,
            width: 0,
            height: 0,
            frame_seq: 0,
            frame_flags: 0,
            qos_state: QoSState::Green,
            tile_record_count: 0,
            capture_ts_us: 0,
            encode_send_delay_us: 0,
            uncompressed_len: 0,
        }
    }
}
impl GDeskFrameHeader {
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        out[0..2].copy_from_slice(&self.magic_version.to_le_bytes());
        out[2..4].copy_from_slice(&self.canvas_epoch.to_le_bytes());
        out[4..6].copy_from_slice(&self.width.to_le_bytes());
        out[6..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_seq.to_le_bytes());
        out[12] = self.frame_flags;
        out[13] = self.qos_state as u8;
        out[14..16].copy_from_slice(&self.tile_record_count.to_le_bytes());
        out[16..24].copy_from_slice(&self.capture_ts_us.to_le_bytes());
        out[24..28].copy_from_slice(&self.encode_send_delay_us.to_le_bytes());
        out[28..32].copy_from_slice(&self.uncompressed_len.to_le_bytes());
        out
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let h = Self {
            magic_version: r.u16()?,
            canvas_epoch: r.u16()?,
            width: r.u16()?,
            height: r.u16()?,
            frame_seq: r.u32()?,
            frame_flags: r.u8()?,
            qos_state: r.u8()?.try_into()?,
            tile_record_count: r.u16()?,
            capture_ts_us: r.u64()?,
            encode_send_delay_us: r.u32()?,
            uncompressed_len: r.u32()?,
        };
        if h.magic_version != MAGIC_VERSION || h.frame_flags & !7 != 0 {
            return Err(Error("invalid frame header"));
        }
        let tiles = tile_count(h.width, h.height)?;
        if h.tile_record_count as usize > tiles
            || h.uncompressed_len as usize > h.tile_record_count as usize * 27000
        {
            return Err(Error("frame exceeds limits"));
        }
        Ok(h)
    }
}

pub fn tile_count(width: u16, height: u16) -> Result<usize> {
    let count = (width as usize).div_ceil(64) * (height as usize).div_ceil(64);
    if width == 0 || height == 0 || count > MAX_TILES {
        return Err(Error("invalid canvas dimensions"));
    }
    Ok(count)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TileRecord {
    CacheHit {
        tile_index: u16,
        slot_id: u16,
    },
    SolidFill {
        tile_index: u16,
        color: [u8; 3],
    },
    TwoColorText {
        tile_index: u16,
        colors: [[u8; 3]; 2],
        mask: Box<[u8; 512]>,
    },
    WaveletInit {
        tile_index: u16,
        shift_y: u8,
        shift_c: u8,
        payload: Vec<u8>,
    },
    WaveletRefine {
        tile_index: u16,
        shift_y: u8,
        shift_c: u8,
        payload: Vec<u8>,
    },
}
impl TileRecord {
    pub fn tile_index(&self) -> u16 {
        match self {
            Self::CacheHit { tile_index, .. }
            | Self::SolidFill { tile_index, .. }
            | Self::TwoColorText { tile_index, .. }
            | Self::WaveletInit { tile_index, .. }
            | Self::WaveletRefine { tile_index, .. } => *tile_index,
        }
    }
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend(self.tile_index().to_le_bytes());
        match self {
            Self::CacheHit { slot_id, .. } => {
                out.extend([1, 0]);
                out.extend(slot_id.to_le_bytes());
            }
            Self::SolidFill { color, .. } => {
                out.extend([2, 0]);
                out.extend(color);
            }
            Self::TwoColorText { colors, mask, .. } => {
                out.extend([3, 0]);
                out.extend(colors[0]);
                out.extend(colors[1]);
                out.extend(mask.iter());
            }
            Self::WaveletInit {
                shift_y,
                shift_c,
                payload,
                ..
            }
            | Self::WaveletRefine {
                shift_y,
                shift_c,
                payload,
                ..
            } => {
                if *shift_y > 15 || *shift_c > 15 || payload.len() > u16::MAX as usize {
                    return Err(Error("invalid wavelet record"));
                }
                out.extend([
                    if matches!(self, Self::WaveletInit { .. }) {
                        4
                    } else {
                        5
                    },
                    shift_y << 4 | shift_c,
                ]);
                out.extend((payload.len() as u16).to_le_bytes());
                out.extend(payload);
            }
        }
        Ok(())
    }
}

pub fn encode_frame(mut header: GDeskFrameHeader, records: &[TileRecord]) -> Result<Vec<u8>> {
    if records.len() > tile_count(header.width, header.height)? {
        return Err(Error("too many tiles"));
    }
    let mut body = Vec::new();
    let mut previous = None;
    for record in records {
        validate_index(
            record.tile_index(),
            &mut previous,
            tile_count(header.width, header.height)?,
        )?;
        record.write(&mut body)?;
    }
    if body.len() > MAX_UNCOMPRESSED_LEN {
        return Err(Error("frame exceeds limits"));
    }
    header.tile_record_count = records.len() as u16;
    header.uncompressed_len = body.len() as u32;
    header.frame_flags |= LZ4_COMPRESSED;
    let mut output = header.to_bytes().to_vec();
    output.extend(lz4_flex::block::compress(&body));
    Ok(output)
}

pub fn is_keyframe_packet(packet: &[u8]) -> bool {
    GDeskFrameHeader::from_bytes(packet)
        .is_ok_and(|header| header.frame_flags & KEYFRAME_RESET != 0)
}

pub fn decode_frame(bytes: &[u8]) -> Result<(GDeskFrameHeader, Vec<TileRecord>)> {
    let header = GDeskFrameHeader::from_bytes(bytes)?;
    let compressed = bytes.get(HEADER_LEN..).ok_or(Error("truncated header"))?;
    if compressed.len() > MAX_UNCOMPRESSED_LEN {
        return Err(Error("frame exceeds limits"));
    }
    let body = if header.frame_flags & LZ4_COMPRESSED != 0 {
        lz4_flex::block::decompress(compressed, header.uncompressed_len as usize)
            .map_err(|_| Error("invalid LZ4 block"))?
    } else {
        if compressed.len() != header.uncompressed_len as usize {
            return Err(Error("invalid body length"));
        }
        compressed.to_vec()
    };
    if body.len() != header.uncompressed_len as usize {
        return Err(Error("invalid body length"));
    }
    let mut r = Reader(&body);
    let mut records = Vec::with_capacity(header.tile_record_count as usize);
    let mut previous = None;
    for _ in 0..header.tile_record_count {
        let tile_index = r.u16()?;
        validate_index(
            tile_index,
            &mut previous,
            tile_count(header.width, header.height)?,
        )?;
        let mode = r.u8()?;
        let param = r.u8()?;
        if mode <= 3 && param != 0 {
            return Err(Error("invalid reserved parameter"));
        }
        records.push(match mode {
            1 => TileRecord::CacheHit {
                tile_index,
                slot_id: r.u16()?,
            },
            2 => TileRecord::SolidFill {
                tile_index,
                color: r.take(3)?.try_into().map_err(|_| Error("invalid color"))?,
            },
            3 => TileRecord::TwoColorText {
                tile_index,
                colors: [
                    r.take(3)?.try_into().map_err(|_| Error("invalid color"))?,
                    r.take(3)?.try_into().map_err(|_| Error("invalid color"))?,
                ],
                mask: Box::new(r.take(512)?.try_into().map_err(|_| Error("invalid mask"))?),
            },
            4 | 5 => {
                let len = r.u16()? as usize;
                let payload = r.take(len)?.to_vec();
                if mode == 4 {
                    TileRecord::WaveletInit {
                        tile_index,
                        shift_y: param >> 4,
                        shift_c: param & 15,
                        payload,
                    }
                } else {
                    TileRecord::WaveletRefine {
                        tile_index,
                        shift_y: param >> 4,
                        shift_c: param & 15,
                        payload,
                    }
                }
            }
            _ => return Err(Error("unknown tile mode")),
        });
    }
    if !r.0.is_empty() {
        return Err(Error("trailing tile data"));
    }
    Ok((header, records))
}
fn validate_index(index: u16, previous: &mut Option<u16>, count: usize) -> Result<()> {
    if index as usize >= count || previous.is_some_and(|p| index <= p) {
        return Err(Error("tile indices must be valid and strictly ascending"));
    }
    *previous = Some(index);
    Ok(())
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self.0.get(..n).ok_or(Error("truncated tile data"))?;
        self.0 = &self.0[n..];
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GDeskAck {
    pub acked_frame_seq: u32,
    pub echo_capture_ts_us: u64,
    pub client_recv_ts_us: u64,
    pub render_queue_depth: u8,
    pub tile_ack_bitmap: Vec<u8>,
}
#[derive(Debug, Default, Clone)]
pub struct GDeskStats {
    pub frames: u64,
    pub encoded_bytes: u64,
    pub cache_hits: u64,
    pub changed_tiles: u64,
    pub refined_tiles: u64,
    pub gpu_encode_us: u64,
    pub gpu_decode_us: u64,
    pub qos_state: QoSState,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_layout_and_safe_parsing() {
        assert_eq!(std::mem::size_of::<GDeskFrameHeader>(), HEADER_LEN);
        let header = GDeskFrameHeader {
            width: 64,
            height: 64,
            capture_ts_us: 123456789,
            frame_seq: 99,
            ..Default::default()
        };
        let bytes = encode_frame(
            header,
            &[TileRecord::SolidFill {
                tile_index: 0,
                color: [1, 2, 3],
            }],
        )
        .unwrap();
        assert_eq!(&bytes[..2], &[0x44, 0x50]);
        assert_eq!(decode_frame(&bytes).unwrap().0.capture_ts_us, 123456789);
        for len in 0..bytes.len() {
            assert!(decode_frame(&bytes[..len]).is_err());
        }
        assert!(!is_keyframe_packet(&bytes));
        let mut keyframe = bytes.clone();
        keyframe[12] |= KEYFRAME_RESET;
        assert!(is_keyframe_packet(&keyframe));
        assert!(!is_keyframe_packet(&keyframe[..HEADER_LEN - 1]));
        let mut wrong_length = bytes.clone();
        let len = u32::from_le_bytes(wrong_length[28..32].try_into().unwrap());
        wrong_length[28..32].copy_from_slice(&(len + 1).to_le_bytes());
        assert_eq!(
            decode_frame(&wrong_length),
            Err(Error("invalid body length"))
        );
        let mut invalid = bytes;
        invalid[0] = 0;
        assert!(decode_frame(&invalid).is_err());
    }
}
