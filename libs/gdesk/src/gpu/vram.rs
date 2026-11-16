use super::{GpuContext, GpuDecoder, GpuEncoder, FrameStats, TILE_WORDS};
use crate::{protocol, Error, Result};
use std::sync::Arc;

const MAGIC: u32 = u32::from_le_bytes(*b"GDVR");
const PACKET_HEADER: usize = 20;
const COEFF_BLOCK: usize = TILE_WORDS as usize * 4;
const FLAG_RESET: u16 = 1;
const FLAG_RESET_KEEP_CACHE: u16 = 2;
const RECORD_MIN: usize = 8;
const RECORD_MAX: usize = 8 + COEFF_BLOCK;

/// A captured frame that already lives in GPU memory. Only the wgpu variants can be consumed by
/// this build; the native variants name the platform objects a capture backend would hand over and
/// are refused until the matching interop import exists.
pub enum VramTextureHandle {
    /// RGBA8, `width * height * 4` bytes, STORAGE usage.
    WgpuBuffer(wgpu::Buffer),
    /// Rgba8Unorm with COPY_SRC usage; `width * 4` must be a multiple of 256.
    WgpuTexture(wgpu::Texture),
    /// Windows: shared handle of an `ID3D11Texture2D`.
    Dxgi { shared_handle: u64, width: u32, height: u32 },
    /// Linux: DMA-BUF plane.
    DmaBuf { fd: i32, stride: u32, modifier: u64, width: u32, height: u32 },
    /// macOS: `IOSurface` id.
    IoSurface { surface_id: u32, width: u32, height: u32 },
}

impl VramTextureHandle {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WgpuBuffer(_) => "wgpu-buffer",
            Self::WgpuTexture(_) => "wgpu-texture",
            Self::Dxgi { .. } => "dxgi",
            Self::DmaBuf { .. } => "dmabuf",
            Self::IoSurface { .. } => "iosurface",
        }
    }
}

enum Source {
    Buffer(usize),
    Texture { slot: usize, texture: wgpu::Texture },
}

pub struct GDeskVramEncoder {
    ctx: Arc<GpuContext>,
    inner: GpuEncoder,
    sources: Vec<Source>,
    epoch: u16,
    seq: u32,
    reset_pending: bool,
    pub last_stats: FrameStats,
}

impl GDeskVramEncoder {
    fn write_header(out: &mut Vec<u8>, epoch: u16, flags: u16, width: u16, height: u16, seq: u32, dirty: usize) {
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(dirty as u32).to_le_bytes());
    }

    pub fn new(ctx: Arc<GpuContext>, width: u16, height: u16) -> Result<Self> {
        Ok(Self {
            inner: GpuEncoder::new(ctx.clone(), width, height)?,
            ctx,
            sources: Vec::new(),
            epoch: 0,
            seq: 0,
            reset_pending: true,
            last_stats: FrameStats::default(),
        })
    }

    pub fn dimensions(&self) -> (u16, u16) {
        self.inner.dimensions()
    }

    pub fn register(&mut self, handle: VramTextureHandle) -> Result<usize> {
        let (width, height) = self.inner.dimensions();
        let source = match handle {
            VramTextureHandle::WgpuBuffer(buffer) => Source::Buffer(self.inner.attach_frame_buffer(buffer)?),
            VramTextureHandle::WgpuTexture(texture) => {
                let size = texture.size();
                if texture.dimension() != wgpu::TextureDimension::D2
                    || texture.depth_or_array_layers() != 1
                    || texture.sample_count() != 1
                    || size.width != width as u32
                    || size.height != height as u32
                    || size.width * 4 % 256 != 0
                    || texture.format() != wgpu::TextureFormat::Rgba8Unorm
                    || !texture.usage().contains(wgpu::TextureUsages::COPY_SRC)
                {
                    return Err(Error("unsupported texture layout"));
                }
                let bytes = width as u64 * height as u64 * 4;
                let slot = self.inner.attach_frame_buffer(self.ctx.storage(bytes))?;
                Source::Texture { slot, texture }
            }
            _ => return Err(Error("native handle import is not available in this build")),
        };
        self.sources.push(source);
        Ok(self.sources.len() - 1)
    }

    /// Re-targets the pipeline to a new resolution. Sources must be registered again; the peer keeps
    /// presenting its old canvas until the first frame of the new epoch has been applied.
    pub fn resize_seamless(&mut self, width: u16, height: u16) -> Result<()> {
        self.inner = GpuEncoder::new(self.ctx.clone(), width, height)?;
        self.sources.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.reset_pending = true;
        Ok(())
    }

    /// Returns `None` when nothing changed since the last frame.
    pub fn encode(&mut self, source: usize) -> Result<Option<Vec<u8>>> {
        let index = match self.sources.get(source).ok_or(Error("unknown frame source"))? {
            Source::Buffer(slot) => *slot,
            Source::Texture { slot, texture } => {
                let size = texture.size();
                let target = self.inner.slot_buffer(*slot).ok_or(Error("missing slot"))?;
                let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
                cmd.copy_texture_to_buffer(
                    texture.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: target,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(size.width * 4),
                            rows_per_image: Some(size.height),
                        },
                    },
                    size,
                );
                self.ctx.queue.submit([cmd.finish()]);
                *slot
            }
        };
        let slot = self.inner.encode_resident(index)?;
        let stats = self.inner.wait_stats(slot)?;
        self.last_stats = stats;
        if stats.dirty_tiles == 0 {
            return Ok(None);
        }
        let raw = self.inner.read_export(stats.dirty_tiles)?;
        let dirty = stats.dirty_tiles as usize;
        let (width, height) = self.inner.dimensions();
        let flags = if self.reset_pending { FLAG_RESET } else { 0 };
        let mut header = Vec::with_capacity(PACKET_HEADER);
        let packet = {
            let list_bytes = dirty * 8;
            let mut body = Vec::with_capacity(list_bytes + dirty * COEFF_BLOCK);
            for tile in 0..dirty {
                let entry = &raw[tile * 8..tile * 8 + 8];
                body.extend_from_slice(entry);
                let solid = entry[2] & 1 != 0;
                if !solid {
                    let start = list_bytes + tile * COEFF_BLOCK;
                    body.extend_from_slice(&raw[start..start + COEFF_BLOCK]);
                }
            }
            Self::write_header(&mut header, self.epoch, flags, width, height, self.seq, dirty);
            header.extend_from_slice(&lz4_flex::block::compress_prepend_size(&body));
            header
        };
        self.seq = self.seq.wrapping_add(1);
        self.reset_pending = false;
        Ok(Some(packet))
    }
}

pub struct GDeskVramDecoder {
    ctx: Arc<GpuContext>,
    inner: Option<GpuDecoder>,
    epoch: u16,
    expected_seq: u32,
    needs_reset: bool,
    pub last_gpu_ns: u64,
}

impl GDeskVramDecoder {
    pub fn new(ctx: Arc<GpuContext>) -> Self {
        Self {
            ctx,
            inner: None,
            epoch: 0,
            expected_seq: 0,
            needs_reset: true,
            last_gpu_ns: 0,
        }
    }

    pub fn dimensions(&self) -> Option<(u16, u16)> {
        self.inner.as_ref().map(|decoder| decoder.dimensions())
    }

    pub fn canvas_epoch(&self) -> u16 {
        self.epoch
    }

    pub fn canvas(&self) -> Result<Vec<u8>> {
        self.inner.as_ref().ok_or(Error("no canvas yet"))?.read_canvas()
    }

    pub fn decoder(&self) -> Option<&GpuDecoder> {
        self.inner.as_ref()
    }

    /// A new epoch is decoded into a fresh canvas and swapped in only once it is complete.
    pub fn decode(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() < PACKET_HEADER + 4
            || u32::from_le_bytes(packet[0..4].try_into().map_err(|_| Error("short"))?) != MAGIC
        {
            return Err(Error("invalid gpu packet"));
        }
        let epoch = u16::from_le_bytes([packet[4], packet[5]]);
        let flags = u16::from_le_bytes([packet[6], packet[7]]);
        let width = u16::from_le_bytes([packet[8], packet[9]]);
        let height = u16::from_le_bytes([packet[10], packet[11]]);
        let seq = u32::from_le_bytes(packet[12..16].try_into().map_err(|_| Error("short"))?);
        let dirty = u32::from_le_bytes(packet[16..20].try_into().map_err(|_| Error("short"))?) as usize;
        if flags & !(FLAG_RESET | FLAG_RESET_KEEP_CACHE) != 0 {
            return Err(Error("unknown gpu packet flags"));
        }
        let tiles = protocol::tile_count(width, height)?;
        if dirty > tiles {
            return Err(Error("too many tiles"));
        }
        let reset = flags & (FLAG_RESET | FLAG_RESET_KEEP_CACHE) != 0;
        if reset {
            if dirty != tiles {
                return Err(Error("incomplete reset frame"));
            }
        } else if self.needs_reset
            || self.inner.is_none()
            || epoch != self.epoch
            || seq != self.expected_seq
            || self.inner.as_ref().map(|d| d.dimensions()) != Some((width, height))
        {
            return Err(Error("frame does not match the canvas"));
        }
        let mut list = vec![0u32; dirty * 2];
        let mut seen = vec![false; tiles];
        let mut check = |tile: usize, header: &[u8]| -> Result<()> {
            let entry = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let index = (entry & 0xFFFF) as usize;
            if index >= tiles || entry >> 17 != 0 || seen[index] {
                return Err(Error("invalid tile entry"));
            }
            seen[index] = true;
            list[tile * 2] = entry;
            list[tile * 2 + 1] = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            Ok(())
        };
        let owned;
        let coeffs: &[u8] = {
            let compressed = &packet[PACKET_HEADER..];
            let declared =
                u32::from_le_bytes(compressed[0..4].try_into().map_err(|_| Error("short"))?) as usize;
            if declared < dirty * RECORD_MIN || declared > dirty * RECORD_MAX {
                return Err(Error("invalid uncompressed size"));
            }
            let body = lz4_flex::block::decompress_size_prepended(compressed)
                .map_err(|_| Error("invalid lz4 body"))?;
            if body.len() != declared {
                return Err(Error("invalid compressed payload"));
            }
            let mut coeffs = vec![0u8; dirty * COEFF_BLOCK];
            let mut at = 0usize;
            for tile in 0..dirty {
                let header = body.get(at..at + 8).ok_or(Error("truncated body"))?;
                check(tile, header)?;
                let entry = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                at += 8;
                if entry >> 16 & 1 == 0 {
                    let block = body.get(at..at + COEFF_BLOCK).ok_or(Error("truncated body"))?;
                    coeffs[tile * COEFF_BLOCK..(tile + 1) * COEFF_BLOCK].copy_from_slice(block);
                    at += COEFF_BLOCK;
                }
            }
            if at != body.len() {
                return Err(Error("trailing bytes"));
            }
            owned = coeffs;
            &owned
        };
        let mut candidate = if reset {
            Some(GpuDecoder::new_standalone(self.ctx.clone(), width, height)?)
        } else {
            None
        };
        let decoder = match candidate.as_mut() {
            Some(decoder) => decoder,
            None => self.inner.as_mut().ok_or(Error("no decoder"))?,
        };
        decoder.load_packet(&list, coeffs, dirty as u32);
        let slot = decoder.decode(false)?;
        self.last_gpu_ns = decoder.wait_ns(slot)?;
        if candidate.is_some() {
            self.inner = candidate;
            self.epoch = epoch;
            self.needs_reset = false;
        }
        self.expected_seq = seq.wrapping_add(1);
        Ok(())
    }
}
