use crate::{protocol, Error, Result};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use wgpu::util::DeviceExt;

pub mod vram;
pub use wgpu;

const TILE_WORDS: u64 = 3 * 2048;

const SCAN_WGSL: &str = r#"
struct Params {
    width: u32,
    height: u32,
    tiles_x: u32,
    tiles_y: u32,
    lossy: u32,
    sharpen: f32,
    force: u32,
    shifts: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read_write> list: array<u32>;
@group(0) @binding(4) var<storage, read_write> args: array<atomic<u32>>;
@group(0) @binding(10) var<storage, read> frame4: array<vec4<u32>>;
@group(0) @binding(11) var<storage, read_write> hashes: array<u32>;

var<workgroup> wg_nonsolid: atomic<u32>;
var<workgroup> wg_hash_a: array<atomic<u32>, 4>;
var<workgroup> wg_hash_b: array<atomic<u32>, 4>;

fn mix32(v: u32) -> u32 {
    var h = v * 0x9E3779B1u;
    h ^= h >> 15u;
    h *= 0x85EBCA77u;
    h ^= h >> 13u;
    return h;
}

@compute @workgroup_size(64, 1, 1)
fn scan_classify(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let w = params.width;
    let h = params.height;
    let w4 = w >> 2u;
    let tile = wid.y * params.tiles_x + wid.x;
    if lid < 4u {
        if lid == 0u { atomicStore(&wg_nonsolid, 0u); }
        atomicStore(&wg_hash_a[lid], 0u);
        atomicStore(&wg_hash_b[lid], 0u);
    }
    workgroupBarrier();
    let first = frame4[wid.y * 64u * w4 + wid.x * 16u].x & 0xFFFFFFu;
    let first4 = vec4<u32>(first);
    let mask = vec4<u32>(0xFFFFFFu);
    let y = wid.y * 64u + lid;
    let base_x4 = wid.x * 16u + ((lid & 3u) << 2u);
    var nonsolid = 0u;
    var acc = vec4<u32>(0u);
    if y < h && base_x4 < w4 {
        for (var k = 0u; k < 4u; k++) {
            let x4 = base_x4 + k;
            let at = y * w4 + x4;
            let px = frame4[at] & mask;
            nonsolid |= u32(any(px != first4));
            acc = ((acc << vec4<u32>(5u)) - acc) + (px ^ vec4<u32>(at));
        }
    }
    if nonsolid != 0u { atomicOr(&wg_nonsolid, 1u); }
    let ha_local = (acc.x ^ (acc.y >> 7u)) + (acc.z ^ (acc.w << 3u)) + lid;
    let hb_local = (acc.y ^ (acc.z >> 5u)) ^ ((acc.w + acc.x) ^ (lid << 9u));
    let bank = lid & 3u;
    atomicAdd(&wg_hash_a[bank], ha_local);
    atomicAdd(&wg_hash_b[bank], hb_local);
    workgroupBarrier();
    if lid == 0u {
        let ha = mix32(atomicLoad(&wg_hash_a[0]) + atomicLoad(&wg_hash_a[1]))
            ^ mix32(atomicLoad(&wg_hash_a[2]) + atomicLoad(&wg_hash_a[3]));
        let hb = mix32(atomicLoad(&wg_hash_b[0]) + atomicLoad(&wg_hash_b[1]))
            ^ mix32(atomicLoad(&wg_hash_b[2]) + atomicLoad(&wg_hash_b[3]));
        let changed = ha != hashes[tile * 2u] || hb != hashes[tile * 2u + 1u];
        if changed || params.force != 0u {
            hashes[tile * 2u] = ha;
            hashes[tile * 2u + 1u] = hb;
            let slot = atomicAdd(&args[0], 1u);
            list[slot * 2u] = tile | select(1u << 16u, 0u, atomicLoad(&wg_nonsolid) != 0u);
            list[slot * 2u + 1u] = first;
        }
    }
}
"#;

fn mallat_axis(position: usize, levels: usize) -> usize {
    let depth = match position {
        p if p & 1 == 1 => 0,
        p if p & 3 == 2 => 1,
        p if p & 7 == 4 => 2,
        _ => 3,
    };
    if depth < levels {
        (32 >> depth) + (position >> (depth + 1))
    } else {
        position >> levels
    }
}

fn low_depth(position: usize) -> usize {
    (position.trailing_zeros() as usize).min(2)
}

// Maps each Mallat-ordered coefficient index to its in-place lifting position in the plane.
fn params_bytes(width: u16, height: u16, tail: [u32; 4]) -> Result<Vec<u8>> {
    let tiles = protocol::tile_count(width, height)?;
    let tiles_x = (width as u32).div_ceil(64);
    Ok([
        width as u32,
        height as u32,
        tiles_x,
        tiles as u32 / tiles_x,
        tail[0],
        tail[1],
        tail[2],
        tail[3],
    ]
    .iter()
    .flat_map(|v| v.to_le_bytes())
    .collect())
}

fn permutation() -> Vec<u32> {
    let mut table = vec![0u32; 4096];
    for y in 0..64 {
        for x in 0..64 {
            let at = mallat_axis(y, low_depth(x) + 1) * 64 + mallat_axis(x, low_depth(y) + 1);
            table[at] = (y * 64 + x) as u32;
        }
    }
    table
}

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_name: String,
    pub backend: String,
    pub timestamps: bool,
    scan_module: wgpu::ShaderModule,
    module: wgpu::ShaderModule,
}

impl GpuContext {
    pub fn new() -> Result<Arc<Self>> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or(Error("no GPU adapter"))?;
        let info = adapter.get_info();
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("gdesk"),
                required_features: if timestamps {
                    wgpu::Features::TIMESTAMP_QUERY
                } else {
                    wgpu::Features::empty()
                },
                required_limits: wgpu::Limits {
                    max_storage_buffer_binding_size: adapter.limits().max_storage_buffer_binding_size,
                    max_buffer_size: adapter.limits().max_buffer_size,
                    max_compute_workgroup_storage_size: adapter.limits().max_compute_workgroup_storage_size,
                    ..wgpu::Limits::default()
                },
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|_| Error("GPU device request failed"))?;
        // SAFETY: every index in shaders.wgsl is derived from tile/lane ids bounded by the
        // buffer sizes allocated here; dropping bounds checks and workgroup zero-fill is what
        // keeps the per-tile wavelet passes inside the frame budget.
        let scan_module = unsafe {
            device.create_shader_module_trusted(
                wgpu::ShaderModuleDescriptor {
                    label: Some("gdesk_scan"),
                    source: wgpu::ShaderSource::Wgsl(SCAN_WGSL.into()),
                },
                wgpu::ShaderRuntimeChecks::unchecked(),
            )
        };
        let module = unsafe {
            device.create_shader_module_trusted(
                wgpu::ShaderModuleDescriptor {
                    label: Some("gdesk"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
                },
                wgpu::ShaderRuntimeChecks::unchecked(),
            )
        };
        Ok(Arc::new(Self {
            device,
            queue,
            adapter_name: info.name,
            backend: format!("{:?}", info.backend),
            timestamps,
            scan_module,
            module,
        }))
    }

    fn pipeline(&self, entry: &str) -> wgpu::ComputePipeline {
        let module = if entry == "scan_classify" {
            &self.scan_module
        } else {
            &self.module
        };
        self.device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
    }

    fn buffer(&self, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size.max(16),
            usage,
            mapped_at_creation: false,
        })
    }

    fn storage(&self, size: u64) -> wgpu::Buffer {
        self.buffer(
            size,
            wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::INDIRECT,
        )
    }

    pub fn create_frame_buffer(&self, rgba: &[u8]) -> wgpu::Buffer {
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("resident frame"),
                contents: rgba,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
            })
    }

    fn params(&self, width: u16, height: u16) -> Result<wgpu::Buffer> {
        Ok(self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: &params_bytes(width, height, [0, 0, 0, 0])?,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            }))
    }

    fn bind(
        &self,
        pipeline: &wgpu::ComputePipeline,
        buffers: &[(u32, &wgpu::Buffer)],
    ) -> wgpu::BindGroup {
        let entries: Vec<_> = buffers
            .iter()
            .map(|(binding, buffer)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        })
    }

    pub fn read_buffer(&self, src: &wgpu::Buffer, size: u64) -> Result<Vec<u8>> {
        let mut ring = StagingRing::new(self, size);
        let slot = ring.acquire()?;
        let mut cmd = self.device.create_command_encoder(&Default::default());
        cmd.copy_buffer_to_buffer(src, 0, ring.buffer(slot), 0, size);
        let idx = self.queue.submit([cmd.finish()]);
        ring.start_map(slot, size, idx);
        ring.read_blocking(&self.device, slot)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Mapping,
}

struct RingSlot {
    buffer: wgpu::Buffer,
    ready: Arc<AtomicBool>,
    error: Arc<AtomicBool>,
    state: SlotState,
    mapped_len: u64,
    sub_idx: Option<wgpu::SubmissionIndex>,
}

pub struct StagingRing {
    slots: [RingSlot; 3],
    next: usize,
}

impl StagingRing {
    pub fn new(ctx: &GpuContext, size: u64) -> Self {
        let slot = || RingSlot {
            buffer: ctx.buffer(
                size,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            ),
            ready: Arc::new(AtomicBool::new(false)),
            error: Arc::new(AtomicBool::new(false)),
            state: SlotState::Free,
            mapped_len: 0,
            sub_idx: None,
        };
        Self {
            slots: [slot(), slot(), slot()],
            next: 0,
        }
    }
    pub fn acquire(&mut self) -> Result<usize> {
        let slot = self.next;
        if self.slots[slot].state != SlotState::Free {
            return Err(Error("staging ring full"));
        }
        self.next = (slot + 1) % self.slots.len();
        Ok(slot)
    }
    pub fn buffer(&self, slot: usize) -> &wgpu::Buffer {
        &self.slots[slot].buffer
    }
    pub fn start_map(&mut self, slot: usize, len: u64, sub_idx: wgpu::SubmissionIndex) {
        let entry = &mut self.slots[slot];
        entry.ready.store(false, Ordering::SeqCst);
        entry.error.store(false, Ordering::SeqCst);
        entry.state = SlotState::Mapping;
        entry.mapped_len = len;
        entry.sub_idx = Some(sub_idx);
        let (ready, error) = (entry.ready.clone(), entry.error.clone());
        entry
            .buffer
            .slice(..len)
            .map_async(wgpu::MapMode::Read, move |result| {
                if result.is_ok() {
                    ready.store(true, Ordering::SeqCst);
                } else {
                    error.store(true, Ordering::SeqCst);
                }
            });
    }
    pub fn try_read(&mut self, device: &wgpu::Device, slot: usize) -> Result<Option<Vec<u8>>> {
        device.poll(wgpu::Maintain::Poll);
        let entry = &mut self.slots[slot];
        if entry.state != SlotState::Mapping {
            return Err(Error("staging slot not mapping"));
        }
        if entry.error.load(Ordering::SeqCst) {
            entry.state = SlotState::Free;
            return Err(Error("staging map failed"));
        }
        if !entry.ready.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let bytes = entry
            .buffer
            .slice(..entry.mapped_len)
            .get_mapped_range()
            .to_vec();
        entry.buffer.unmap();
        entry.state = SlotState::Free;
        Ok(Some(bytes))
    }
    pub fn read_blocking(&mut self, device: &wgpu::Device, slot: usize) -> Result<Vec<u8>> {
        if let Some(sub_idx) = self.slots[slot].sub_idx.take() {
            device.poll(wgpu::Maintain::WaitForSubmissionIndex(sub_idx));
        } else {
            device.poll(wgpu::Maintain::Wait);
        }
        self.try_read(device, slot)?
            .ok_or(Error("staging map incomplete"))
    }
}

struct Timer {
    set: Option<wgpu::QuerySet>,
    resolve: Option<wgpu::Buffer>,
    count: u32,
}

impl Timer {
    fn new(ctx: &GpuContext, count: u32) -> Self {
        if !ctx.timestamps {
            return Self {
                set: None,
                resolve: None,
                count,
            };
        }
        let total = count * 3;
        Self {
            set: Some(ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: None,
                ty: wgpu::QueryType::Timestamp,
                count: total,
            })),
            resolve: Some(ctx.buffer(
                3 * 256,
                wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            )),
            count,
        }
    }
    fn pass(&self, slot: usize, begin: u32) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        let base = slot as u32 * self.count + begin;
        self.set
            .as_ref()
            .map(|set| wgpu::ComputePassTimestampWrites {
                query_set: set,
                beginning_of_pass_write_index: Some(base),
                end_of_pass_write_index: Some(base + 1),
            })
    }
    fn resolve_into(&self, cmd: &mut wgpu::CommandEncoder, slot: usize, ring: &wgpu::Buffer, offset: u64) {
        if let (Some(set), Some(resolve)) = (&self.set, &self.resolve) {
            let base = slot as u32 * self.count;
            let byte_off = slot as u64 * 256;
            cmd.resolve_query_set(set, base..base + self.count, resolve, byte_off);
            cmd.copy_buffer_to_buffer(resolve, byte_off, ring, offset, self.count as u64 * 8);
        }
    }
    fn nanos(&self, ctx: &GpuContext, bytes: &[u8], fallback: Instant) -> u64 {
        if self.set.is_none() {
            return fallback.elapsed().as_nanos() as u64;
        }
        let mut prev_end = 0u64;
        let mut busy = 0u64;
        for pair in bytes.chunks_exact(16) {
            let begin = u64::from_le_bytes(pair[..8].try_into().unwrap_or([0; 8])).max(prev_end);
            let end = u64::from_le_bytes(pair[8..].try_into().unwrap_or([0; 8]));
            busy += end.saturating_sub(begin);
            prev_end = prev_end.max(end);
        }
        (busy as f64 * ctx.queue.get_timestamp_period() as f64) as u64
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    pub dirty_tiles: u32,
    pub gpu_ns: u64,
    pub scan_ns: u64,
}

#[derive(Debug, Clone)]
pub struct GpuTile {
    pub tile: u32,
    pub solid: Option<[u8; 3]>,
    pub planes: [Vec<i32>; 3],
}

struct FrameSlot {
    frame: wgpu::Buffer,
    scan_binds: [wgpu::BindGroup; 3],
    pack_binds: [wgpu::BindGroup; 3],
}

struct PipeOut {
    list: wgpu::Buffer,
    args: wgpu::Buffer,
    coeffs: wgpu::Buffer,
}

pub struct GpuEncoder {
    ctx: Arc<GpuContext>,
    width: u16,
    height: u16,
    tiles: usize,
    slots: Vec<FrameSlot>,
    prev: wgpu::Buffer,
    outs: [PipeOut; 3],
    last_ring_slot: usize,
    params: wgpu::Buffer,
    scan: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    shifts: (u8, u8),
    force: bool,
    last_tail: [u32; 2],
    timer: Timer,
    ring: StagingRing,
    export_ring: Option<StagingRing>,
    submitted: Instant,
}

impl GpuEncoder {
    pub fn new(ctx: Arc<GpuContext>, width: u16, height: u16) -> Result<Self> {
        let tiles = protocol::tile_count(width, height)?;
        if width % 4 != 0 {
            return Err(Error("GPU path requires width divisible by 4"));
        }
        let prev = ctx.storage(tiles as u64 * 8);
        ctx.queue.write_buffer(&prev, 0, &vec![0xFF; tiles * 8]);
        const INIT_ARGS: [u8; 16] = [0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let make_out = || {
            let args = ctx.storage(16);
            ctx.queue.write_buffer(&args, 0, &INIT_ARGS);
            PipeOut {
                list: ctx.storage(tiles as u64 * 8),
                args,
                coeffs: ctx.storage(tiles as u64 * TILE_WORDS * 4),
            }
        };
        Ok(Self {
            prev,
            outs: [make_out(), make_out(), make_out()],
            last_ring_slot: 0,
            params: ctx.params(width, height)?,
            scan: ctx.pipeline("scan_classify"),
            pack: ctx.pipeline("wavelet_pack"),
            timer: Timer::new(&ctx, 4),
            ring: StagingRing::new(&ctx, 48),
            ctx,
            width,
            height,
            tiles,
            slots: Vec::new(),
            shifts: (0, 0),
            force: false,
            last_tail: [u32::MAX, u32::MAX],
            export_ring: None,
            submitted: Instant::now(),
        })
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn slot_buffer(&self, index: usize) -> Option<&wgpu::Buffer> {
        self.slots.get(index).map(|slot| &slot.frame)
    }

    fn make_slot(&self, frame: wgpu::Buffer) -> FrameSlot {
        let scan_binds = std::array::from_fn(|s| {
            self.ctx.bind(
                &self.scan,
                &[
                    (0, &self.params),
                    (10, &frame),
                    (11, &self.prev),
                    (3, &self.outs[s].list),
                    (4, &self.outs[s].args),
                ],
            )
        });
        let pack_binds = std::array::from_fn(|s| {
            self.ctx.bind(
                &self.pack,
                &[
                    (0, &self.params),
                    (1, &frame),
                    (3, &self.outs[s].list),
                    (5, &self.outs[s].coeffs),
                ],
            )
        });
        FrameSlot {
            frame,
            scan_binds,
            pack_binds,
        }
    }

    /// Adopts a frame that already lives in GPU memory; no pixel data crosses the CPU.
    pub fn attach_frame_buffer(&mut self, buffer: wgpu::Buffer) -> Result<usize> {
        let bytes = self.width as u64 * self.height as u64 * 4;
        if buffer.size() < bytes || !buffer.usage().contains(wgpu::BufferUsages::STORAGE) {
            return Err(Error("frame buffer too small or not a storage buffer"));
        }
        let slot = self.make_slot(buffer);
        self.slots.push(slot);
        Ok(self.slots.len() - 1)
    }

    /// Reads back the tile list and coefficient blocks of the latest encoded frame.
    pub fn read_export(&mut self, dirty_tiles: u32) -> Result<Vec<u8>> {
        if dirty_tiles as usize > self.tiles as usize {
            return Err(Error("dirty tiles exceed the frame"));
        }
        let list_bytes = dirty_tiles as u64 * 8;
        let coeff_bytes = dirty_tiles as u64 * TILE_WORDS * 4;
        if self.export_ring.is_none() {
            self.export_ring = Some(StagingRing::new(
                &self.ctx,
                self.tiles as u64 * (8 + TILE_WORDS * 4),
            ));
        }
        let out = &self.outs[self.last_ring_slot];
        let ring = self.export_ring.as_mut().ok_or(Error("no export ring"))?;
        let slot = ring.acquire()?;
        let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
        cmd.copy_buffer_to_buffer(&out.list, 0, ring.buffer(slot), 0, list_bytes);
        cmd.copy_buffer_to_buffer(&out.coeffs, 0, ring.buffer(slot), list_bytes, coeff_bytes);
        let idx = self.ctx.queue.submit([cmd.finish()]);
        ring.start_map(slot, list_bytes + coeff_bytes, idx);
        ring.read_blocking(&self.ctx.device, slot)
    }

    pub fn set_quality(&mut self, shift_y: u8, shift_c: u8, force: bool) {
        self.shifts = (shift_y.min(15), shift_c.min(15));
        self.force = force;
    }

    pub fn upload_frame_slot(&mut self, index: usize, rgba: &[u8]) -> Result<()> {
        let bytes = self.width as u64 * self.height as u64 * 4;
        if rgba.len() as u64 != bytes {
            return Err(Error("invalid RGBA frame size"));
        }
        if index > self.slots.len() {
            return Err(Error("frame slots must be uploaded in order"));
        }
        if index == self.slots.len() {
            let slot = self.make_slot(self.ctx.storage(bytes));
            self.slots.push(slot);
        }
        self.ctx.queue.write_buffer(&self.slots[index].frame, 0, rgba);
        Ok(())
    }

    pub fn encode(&mut self, rgba: &[u8]) -> Result<usize> {
        self.upload_frame_slot(0, rgba)?;
        self.encode_resident(0)
    }

    pub fn encode_resident(&mut self, index: usize) -> Result<usize> {
        let frame = self.slots.get(index).ok_or(Error("frame slot not uploaded"))?;
        let slot = self.ring.acquire()?;
        self.last_ring_slot = slot;
        let out = &self.outs[slot];
        let (tiles_x, tiles_y) = (
            (self.width as u32).div_ceil(64),
            (self.height as u32).div_ceil(64),
        );
        let cur_tail = [
            self.force as u32,
            self.shifts.0 as u32 | (self.shifts.1 as u32) << 8,
        ];
        if self.last_tail != cur_tail {
            self.last_tail = cur_tail;
            let mut params = params_bytes(self.width, self.height, [0, 0, cur_tail[0], cur_tail[1]])?;
            params.drain(..16);
            self.ctx.queue.write_buffer(&self.params, 16, &params);
        }
        let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
        cmd.clear_buffer(&out.args, 0, Some(4));
        {
            let mut pass = cmd.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("scan_classify"),
                timestamp_writes: self.timer.pass(slot, 0),
            });
            pass.set_pipeline(&self.scan);
            pass.set_bind_group(0, &frame.scan_binds[slot], &[]);
            pass.dispatch_workgroups(tiles_x, tiles_y, 1);
        }
        {
            let mut pass = cmd.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("wavelet_pack"),
                timestamp_writes: self.timer.pass(slot, 2),
            });
            pass.set_pipeline(&self.pack);
            pass.set_bind_group(0, &frame.pack_binds[slot], &[]);
            pass.dispatch_workgroups_indirect(&out.args, 0);
        }
        cmd.copy_buffer_to_buffer(&out.args, 0, self.ring.buffer(slot), 0, 4);
        self.timer.resolve_into(&mut cmd, slot, self.ring.buffer(slot), 16);
        self.submitted = Instant::now();
        let idx = self.ctx.queue.submit([cmd.finish()]);
        self.ring.start_map(slot, 48, idx);
        Ok(slot)
    }

    fn parse_stats(&self, bytes: &[u8]) -> Result<FrameStats> {
        Ok(FrameStats {
            dirty_tiles: u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| Error("bad stats"))?),
            gpu_ns: self.timer.nanos(&self.ctx, &bytes[16..48], self.submitted),
            scan_ns: self.timer.nanos(&self.ctx, &bytes[16..32], self.submitted),
        })
    }

    pub fn wait_stats(&mut self, slot: usize) -> Result<FrameStats> {
        let bytes = self.ring.read_blocking(&self.ctx.device, slot)?;
        self.parse_stats(&bytes)
    }

    pub fn try_poll_stats(&mut self, slot: usize) -> Result<Option<FrameStats>> {
        match self.ring.try_read(&self.ctx.device, slot)? {
            Some(bytes) => Ok(Some(self.parse_stats(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn read_tiles(&self, dirty_tiles: u32) -> Result<Vec<GpuTile>> {
        let out = &self.outs[self.last_ring_slot];
        let words = |bytes: Vec<u8>| -> Vec<u32> {
            bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                .collect()
        };
        let list = words(self.ctx.read_buffer(&out.list, self.tiles as u64 * 8)?);
        let coeffs = words(
            self.ctx
                .read_buffer(&out.coeffs, self.tiles as u64 * TILE_WORDS * 4)?,
        );
        let perm = permutation();
        Ok((0..dirty_tiles as usize)
            .map(|slot| {
                let entry = list[slot * 2];
                let color = list[slot * 2 + 1];
                let plane = |p: usize| {
                    let at = (slot * 3 + p) * 2048;
                    let inplace: Vec<i32> = coeffs[at..at + 2048]
                        .iter()
                        .flat_map(|w| [*w as u16 as i16 as i32, (*w >> 16) as u16 as i16 as i32])
                        .collect();
                    perm.iter().map(|&idx| inplace[idx as usize]).collect::<Vec<i32>>()
                };
                let solid = (entry >> 16 & 1 != 0)
                    .then_some([color as u8, (color >> 8) as u8, (color >> 16) as u8]);
                GpuTile {
                    tile: entry & 0xFFFF,
                    solid,
                    planes: if solid.is_some() {
                        Default::default()
                    } else {
                        [plane(0), plane(1), plane(2)]
                    },
                }
            })
            .collect())
    }
}

pub struct GpuDecoder {
    ctx: Arc<GpuContext>,
    width: u16,
    height: u16,
    params: wgpu::Buffer,
    canvas: wgpu::Buffer,
    display: wgpu::Buffer,
    wstate: wgpu::Buffer,
    args: [wgpu::Buffer; 3],
    list: wgpu::Buffer,
    coeffs: wgpu::Buffer,
    resample: wgpu::ComputePipeline,
    decode: wgpu::ComputePipeline,
    present: wgpu::ComputePipeline,
    decode_binds: [wgpu::BindGroup; 3],
    present_bind: wgpu::BindGroup,
    last_params: [u32; 2],
    timer: Timer,
    ring: StagingRing,
    submitted: Instant,
}

impl GpuDecoder {
    pub fn new(ctx: Arc<GpuContext>, encoder: &GpuEncoder) -> Result<Self> {
        Self::build(
            ctx,
            encoder.width,
            encoder.height,
            std::array::from_fn(|s| {
                (
                    encoder.outs[s].list.clone(),
                    encoder.outs[s].coeffs.clone(),
                    encoder.outs[s].args.clone(),
                )
            }),
        )
    }

    /// A decoder that owns its tile list and coefficient buffers and is fed from packets.
    pub fn new_standalone(ctx: Arc<GpuContext>, width: u16, height: u16) -> Result<Self> {
        let tiles = protocol::tile_count(width, height)? as u64;
        let list = ctx.storage(tiles * 8);
        let coeffs = ctx.storage(tiles * TILE_WORDS * 4);
        let args = ctx.storage(16);
        Self::build(
            ctx,
            width,
            height,
            std::array::from_fn(|_| (list.clone(), coeffs.clone(), args.clone())),
        )
    }

    fn build(
        ctx: Arc<GpuContext>,
        width: u16,
        height: u16,
        outs: [(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer); 3],
    ) -> Result<Self> {
        let tiles = protocol::tile_count(width, height)? as u64;
        let pixels = width as u64 * height as u64 * 4;
        let params = ctx.params(width, height)?;
        let canvas = ctx.storage(pixels);
        let display = ctx.storage(pixels);
        let wstate = ctx.storage(tiles * TILE_WORDS * 4);
        let flags = ctx.storage(tiles * 4);
        let decode = ctx.pipeline("wavelet_decode");
        let present = ctx.pipeline("present");
        let ctx_resample = ctx.pipeline("resample_sharpen");
        let decode_binds = std::array::from_fn(|s| {
            ctx.bind(
                &decode,
                &[
                    (0, &params),
                    (3, &outs[s].0),
                    (5, &outs[s].1),
                    (6, &canvas),
                    (7, &wstate),
                    (8, &flags),
                ],
            )
        });
        let present_bind = ctx.bind(
            &present,
            &[(0, &params), (6, &canvas), (8, &flags), (9, &display)],
        );
        Ok(Self {
            timer: Timer::new(&ctx, 2),
            ring: StagingRing::new(&ctx, 16),
            ctx,
            width,
            height,
            params,
            canvas,
            display,
            wstate,
            args: std::array::from_fn(|s| outs[s].2.clone()),
            list: outs[0].0.clone(),
            coeffs: outs[0].1.clone(),
            resample: ctx_resample,
            decode,
            present,
            decode_binds,
            present_bind,
            last_params: [u32::MAX, u32::MAX],
            submitted: Instant::now(),
        })
    }

    fn set_params(&mut self, lossy: bool, sharpen: f32) -> Result<()> {
        let cur = [lossy as u32, sharpen.to_bits()];
        if self.last_params != cur {
            self.last_params = cur;
            let bytes = params_bytes(self.width, self.height, [cur[0], cur[1], 0, 0])?;
            self.ctx.queue.write_buffer(&self.params, 0, &bytes);
        }
        Ok(())
    }

    pub fn decode(&mut self, lossy: bool) -> Result<usize> {
        let slot = self.ring.acquire()?;
        self.set_params(lossy, 0.0)?;
        let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
        {
            let mut pass = cmd.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("wavelet_decode"),
                timestamp_writes: self.timer.pass(slot, 0),
            });
            pass.set_pipeline(&self.decode);
            pass.set_bind_group(0, &self.decode_binds[slot], &[]);
            pass.dispatch_workgroups_indirect(&self.args[slot], 0);
        }
        self.timer.resolve_into(&mut cmd, slot, self.ring.buffer(slot), 0);
        self.submitted = Instant::now();
        let idx = self.ctx.queue.submit([cmd.finish()]);
        self.ring.start_map(slot, 16, idx);
        Ok(slot)
    }

    pub fn wait_ns(&mut self, slot: usize) -> Result<u64> {
        let bytes = self.ring.read_blocking(&self.ctx.device, slot)?;
        Ok(self.timer.nanos(&self.ctx, &bytes, self.submitted))
    }

    pub fn try_poll_ns(&mut self, slot: usize) -> Result<Option<u64>> {
        Ok(self
            .ring
            .try_read(&self.ctx.device, slot)?
            .map(|bytes| self.timer.nanos(&self.ctx, &bytes, self.submitted)))
    }

    pub fn present(&mut self, sharpen: f32) -> Result<Vec<u8>> {
        self.set_params(false, sharpen)?;
        let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
        {
            let mut pass = cmd.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.present);
            pass.set_bind_group(0, &self.present_bind, &[]);
            pass.dispatch_workgroups(
                (self.width as u32).div_ceil(16),
                (self.height as u32).div_ceil(16),
                1,
            );
        }
        self.ctx.queue.submit([cmd.finish()]);
        self.read(&self.display)
    }

    pub fn read_canvas(&self) -> Result<Vec<u8>> {
        self.read(&self.canvas)
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn load_packet(&self, list: &[u32], coeffs: &[u8], dirty: u32) {
        let bytes: Vec<u8> = list.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.ctx.queue.write_buffer(&self.list, 0, &bytes);
        self.ctx.queue.write_buffer(&self.coeffs, 0, coeffs);
        let args: Vec<u8> = [dirty, 1, 1, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        self.ctx.queue.write_buffer(&self.args[0], 0, &args);
    }

    /// Bridges a canvas decoded elsewhere (for example by the CPU codec) into GPU memory.
    pub fn upload_canvas(&self, rgba: &[u8]) -> Result<()> {
        if rgba.len() as u64 != self.width as u64 * self.height as u64 * 4 {
            return Err(Error("invalid canvas size"));
        }
        self.ctx.queue.write_buffer(&self.canvas, 0, rgba);
        Ok(())
    }

    /// Lanczos-2 resampling followed by contrast-adaptive sharpening, for a display widget whose
    /// physical size differs from the canvas. Never falls back to bilinear.
    pub fn resample_sharpen(&self, dst_w: u32, dst_h: u32, sharpness: f32) -> Result<Vec<u8>> {
        if dst_w == 0 || dst_h == 0 || dst_w > 8192 || dst_h > 8192 {
            return Err(Error("invalid target size"));
        }
        let bytes = dst_w as u64 * dst_h as u64 * 4;
        let scaled = self.ctx.storage(bytes);
        let params: Vec<u8> = [
            self.width as u32,
            self.height as u32,
            dst_w,
            dst_h,
            sharpness.to_bits(),
            0,
            0,
            0,
        ]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
        let params = self
            .ctx
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("resample params"),
                contents: &params,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind = self
            .ctx
            .bind(&self.resample, &[(6, &self.canvas), (13, &scaled), (14, &params)]);
        let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
        {
            let mut pass = cmd.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.resample);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(dst_w.div_ceil(16), dst_h.div_ceil(16), 1);
        }
        self.ctx.queue.submit([cmd.finish()]);
        self.ctx.read_buffer(&scaled, bytes)
    }

    pub fn wavelet_state(&self) -> Result<Vec<u32>> {
        let tiles = protocol::tile_count(self.width, self.height)? as u64;
        let raw = self.ctx.read_buffer(&self.wstate, tiles * TILE_WORDS * 4)?;
        Ok(raw
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
            .collect())
    }

    fn read(&self, buffer: &wgpu::Buffer) -> Result<Vec<u8>> {
        self.ctx
            .read_buffer(buffer, self.width as u64 * self.height as u64 * 4)
    }
}
