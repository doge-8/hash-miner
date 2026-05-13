use anyhow::{anyhow, Context, Result};
use bytemuck::{Pod, Zeroable};
use std::time::Instant;

const WORKGROUP_SIZE: u32 = 64;
const ITERATIONS_PER_THREAD: u32 = 16;
const NONCES_PER_WORKGROUP: u64 = (WORKGROUP_SIZE * ITERATIONS_PER_THREAD) as u64;
pub const MAX_WORKGROUPS_PER_DISPATCH: u32 = 65535;
pub const MAX_BATCH_SIZE: u64 = (MAX_WORKGROUPS_PER_DISPATCH as u64) * NONCES_PER_WORKGROUP;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
struct Uniforms {
    challenge: [u32; 8],
    difficulty: [u32; 8],
    nonce_base_lo: u32,
    nonce_base_hi: u32,
    _pad0: u32,
    _pad1: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
struct ResultBuffer {
    found: u32,
    nonce_lo: u32,
    nonce_hi: u32,
    _pad: u32,
    hash: [u32; 8],
}

/// 一条 pipelining 流水线: 单独的 uniform / result / readback / bind_group.
/// 流水线越多, 同时在 GPU 队列里跑的 batch 越多, 利用率越高.
struct Slot {
    uniform_buf: wgpu::Buffer,
    result_buf: wgpu::Buffer,
    readback_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

pub struct GpuMiner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    pub adapter_name: String,
    pub backend: String,
    /// GPU 指纹, 用于检测换卡后是否需要重测
    pub fingerprint: String,
}

#[derive(Clone, Copy)]
pub struct MineHit {
    pub nonce: u64,
    pub hash_be: [u8; 32],
}

pub struct MineProgress {
    pub nonces_tried: u64,
    pub hashrate_mhs: f64,
}

pub struct BenchResult {
    pub batch_size: u64,
    pub pipeline_depth: u32,
    pub hashrate_mhs: f64,
}

impl GpuMiner {
    /// 单 GPU 模式: 自动选最高性能 adapter (兼容旧用法).
    #[allow(dead_code)]
    pub async fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .ok_or_else(|| anyhow!("找不到可用 GPU adapter"))?;
        Self::from_adapter(adapter).await
    }

    /// 枚举所有可用 GPU. 跳过软渲染 (CPU 后端) 和虚拟设备.
    pub async fn enumerate_all() -> Result<Vec<Self>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapters = instance.enumerate_adapters(wgpu::Backends::PRIMARY);
        eprintln!("[GPU] wgpu 枚举到 {} 个 adapter", adapters.len());
        let mut miners = Vec::new();
        for adapter in adapters {
            let info = adapter.get_info();
            eprintln!(
                "[GPU] 候选: name={:?} type={:?} backend={:?} vendor=0x{:x} device=0x{:x}",
                info.name, info.device_type, info.backend, info.vendor, info.device
            );
            // 过滤: 跳过 CPU 软渲染
            if matches!(
                info.device_type,
                wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
            ) {
                eprintln!("[GPU]   → 跳过 (device_type 不是 GPU)");
                continue;
            }
            match Self::from_adapter(adapter).await {
                Ok(m) => {
                    eprintln!("[GPU]   → 已加入");
                    miners.push(m);
                }
                Err(e) => eprintln!("[GPU]   → 跳过 ({}: {})", info.backend.to_str(), e),
            }
        }
        if miners.is_empty() {
            return Err(anyhow!("找不到任何可用 GPU"));
        }
        Ok(miners)
    }

    /// 从一个 Adapter 构造 GpuMiner (共享 shader 模块和管线).
    pub async fn from_adapter(adapter: wgpu::Adapter) -> Result<Self> {
        let info = adapter.get_info();
        let adapter_name = info.name.clone();
        let backend = format!("{:?}", info.backend);
        let fingerprint = format!("{}|{}|{:?}", info.name, info.vendor, info.backend);

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("hash-miner-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits {
                        max_compute_workgroups_per_dimension: MAX_WORKGROUPS_PER_DISPATCH,
                        ..wgpu::Limits::downlevel_defaults()
                    },
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .context("申请 GPU device 失败")?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("keccak.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/keccak.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("keccak-pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "main",
            compilation_options: Default::default(),
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            adapter_name,
            backend,
            fingerprint,
        })
    }

    fn make_slot(&self) -> Slot {
        let uniform_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let result_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("result"),
            size: std::mem::size_of::<ResultBuffer>() as u64,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: std::mem::size_of::<ResultBuffer>() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: result_buf.as_entire_binding(),
                },
            ],
        });
        Slot {
            uniform_buf,
            result_buf,
            readback_buf,
            bind_group,
        }
    }

    /// 把 batch_size 对齐到 NONCES_PER_WORKGROUP 整数倍, 并 clamp 到上限.
    pub fn normalize_batch(hint: u64) -> (u64, u32) {
        let wg = ((hint + NONCES_PER_WORKGROUP - 1) / NONCES_PER_WORKGROUP)
            .max(1)
            .min(MAX_WORKGROUPS_PER_DISPATCH as u64) as u32;
        ((wg as u64) * NONCES_PER_WORKGROUP, wg)
    }

    /// 跑一次 dispatch, 计时. 用于 benchmark.
    pub async fn bench_once(&self, batch_size: u64) -> Result<f64> {
        let (_real_batch, workgroup_count) = Self::normalize_batch(batch_size);
        let slot = self.make_slot();
        // 难度设为最小值 (前 31 字节 0 + 最后一字节 0x01), 几乎永不命中, 强制全 batch 跑完.
        let mut diff = [0u8; 32];
        diff[31] = 1;
        let challenge = [0x77u8; 32];
        let uniforms = make_uniforms(&challenge, &diff, 0);

        self.queue
            .write_buffer(&slot.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        self.queue.write_buffer(
            &slot.result_buf,
            0,
            bytemuck::bytes_of(&ResultBuffer::default()),
        );

        let start = Instant::now();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("bench"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &slot.bind_group, &[]);
            pass.dispatch_workgroups(workgroup_count, 1, 1);
        }
        self.queue.submit(Some(encoder.finish()));
        // 强制 GPU 同步完成, 才能拿真实耗时
        self.device.poll(wgpu::Maintain::Wait);
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }

    /// 跑 pipelined benchmark: 同时排队 depth 个 batch, 测稳态吞吐.
    /// 跑 N 个完整 batch 计时, 不用 poll Wait 在中间打断, 让 GPU 队列连续跑.
    pub async fn bench_pipelined(
        &self,
        batch_size: u64,
        pipeline_depth: u32,
        warmup_batches: u32,
        measure_batches: u32,
    ) -> Result<f64> {
        let (real_batch, workgroup_count) = Self::normalize_batch(batch_size);
        let depth = pipeline_depth.max(1) as usize;
        let slots: Vec<Slot> = (0..depth).map(|_| self.make_slot()).collect();

        let mut diff = [0u8; 32];
        diff[31] = 1;
        let challenge = [0x77u8; 32];
        let mut nonce_base: u64 = 0;

        // warmup: 提交 warmup_batches 个 batch 让 GPU 进入稳态
        for i in 0..warmup_batches {
            let slot = &slots[(i as usize) % depth];
            self.submit_dispatch(slot, &challenge, &diff, nonce_base, workgroup_count);
            nonce_base = nonce_base.wrapping_add(real_batch);
        }
        self.device.poll(wgpu::Maintain::Wait);

        // measure: 提交 measure_batches 个并测时
        let start = Instant::now();
        for i in 0..measure_batches {
            let slot = &slots[(i as usize) % depth];
            self.submit_dispatch(slot, &challenge, &diff, nonce_base, workgroup_count);
            nonce_base = nonce_base.wrapping_add(real_batch);
        }
        self.device.poll(wgpu::Maintain::Wait);
        let elapsed = start.elapsed().as_secs_f64().max(1e-6);
        let total_nonces = measure_batches as f64 * real_batch as f64;
        Ok(total_nonces / elapsed / 1_000_000.0)
    }

    fn submit_dispatch(
        &self,
        slot: &Slot,
        challenge: &[u8; 32],
        difficulty: &[u8; 32],
        nonce_base: u64,
        workgroup_count: u32,
    ) {
        let u = make_uniforms(challenge, difficulty, nonce_base);
        self.queue
            .write_buffer(&slot.uniform_buf, 0, bytemuck::bytes_of(&u));
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("dispatch"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &slot.bind_group, &[]);
            pass.dispatch_workgroups(workgroup_count, 1, 1);
        }
        self.queue.submit(Some(enc.finish()));
    }

    /// 真实挖矿: pipelined 多 slot 持续搜.
    /// challenge: 32 bytes (LE lane interpretation, raw from getChallenge).
    /// difficulty: 32 bytes BE.
    pub async fn search(
        &self,
        batch_size: u64,
        pipeline_depth: u32,
        challenge: [u8; 32],
        difficulty: [u8; 32],
        start_nonce: u64,
        mut should_stop: impl FnMut() -> bool,
        mut progress: impl FnMut(MineProgress),
    ) -> Result<Option<MineHit>> {
        let (real_batch, workgroup_count) = Self::normalize_batch(batch_size);
        let depth = pipeline_depth.max(1) as usize;
        let slots: Vec<Slot> = (0..depth).map(|_| self.make_slot()).collect();

        let mut nonce_cursor = start_nonce;
        let start_time = Instant::now();
        let mut total_nonces: u64 = 0;
        let zero_result = ResultBuffer::default();

        // 初始填满所有 slot
        for slot in &slots {
            self.queue.write_buffer(
                &slot.result_buf,
                0,
                bytemuck::bytes_of(&zero_result),
            );
            self.submit_dispatch(slot, &challenge, &difficulty, nonce_cursor, workgroup_count);
            nonce_cursor = nonce_cursor.wrapping_add(real_batch);
        }

        loop {
            if should_stop() {
                return Ok(None);
            }

            // 轮询每个 slot: 完成一个就回读, 然后再投一个新的进去
            for slot_idx in 0..depth {
                if should_stop() {
                    return Ok(None);
                }
                let slot = &slots[slot_idx];

                // 复制 result -> readback
                let mut enc = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                enc.copy_buffer_to_buffer(
                    &slot.result_buf,
                    0,
                    &slot.readback_buf,
                    0,
                    std::mem::size_of::<ResultBuffer>() as u64,
                );
                self.queue.submit(Some(enc.finish()));

                let slice = slot.readback_buf.slice(..);
                let (tx, rx) = tokio::sync::oneshot::channel();
                slice.map_async(wgpu::MapMode::Read, move |res| {
                    let _ = tx.send(res);
                });
                self.device.poll(wgpu::Maintain::Wait);
                rx.await
                    .context("GPU map_async 通道关闭")?
                    .context("GPU 回读失败")?;

                let data = slice.get_mapped_range();
                let result: ResultBuffer = *bytemuck::from_bytes(&data);
                drop(data);
                slot.readback_buf.unmap();

                total_nonces = total_nonces.saturating_add(real_batch);
                let elapsed = start_time.elapsed().as_secs_f64().max(1e-6);
                progress(MineProgress {
                    nonces_tried: total_nonces,
                    hashrate_mhs: (total_nonces as f64) / elapsed / 1_000_000.0,
                });

                if result.found > 0 {
                    let nonce = ((result.nonce_hi as u64) << 32) | (result.nonce_lo as u64);
                    let mut hash_be = [0u8; 32];
                    for i in 0..8 {
                        hash_be[i * 4..i * 4 + 4].copy_from_slice(&result.hash[i].to_be_bytes());
                    }
                    return Ok(Some(MineHit { nonce, hash_be }));
                }

                // 重置 result 后再投下一个 batch
                self.queue.write_buffer(
                    &slot.result_buf,
                    0,
                    bytemuck::bytes_of(&zero_result),
                );
                self.submit_dispatch(slot, &challenge, &difficulty, nonce_cursor, workgroup_count);
                nonce_cursor = nonce_cursor.wrapping_add(real_batch);
            }
        }
    }

    /// auto_tune: 实测每个候选 batch_size 的 hashrate, 选最高的.
    /// target_ms 只用来给单 batch 设上限 (避免单 batch 太长拖慢 watcher 响应).
    pub async fn auto_tune(&self, target_ms: f64) -> Result<BenchResult> {
        let warmup_n = 5u32;
        let measure_n = 20u32;

        // 候选 batch: 从 1M 倍增到上限或单 batch 超过 target_ms × 2
        let mut candidates: Vec<u64> = Vec::new();
        let mut b: u64 = 1 << 20;
        while b <= MAX_BATCH_SIZE {
            candidates.push(b);
            if b * 2 > MAX_BATCH_SIZE {
                break;
            }
            b *= 2;
        }

        let mut best_batch = candidates[0];
        let mut best_mhs = 0.0f64;
        let mut best_ms = 0.0f64;
        for &b in &candidates {
            let ms = self.bench_once(b).await?;
            // 单 batch 超过 target_ms × 2.5 直接丢, 避免响应过慢
            if ms > target_ms * 2.5 && best_mhs > 0.0 {
                println!(
                    "  · batch={:>5}M → {:>6.1} ms/batch · 超 {:.0}ms 上限, 跳过",
                    b / 1_000_000,
                    ms,
                    target_ms * 2.5
                );
                break;
            }
            // pipelined 实测 hashrate (depth=1)
            let mhs = self.bench_pipelined(b, 1, warmup_n, measure_n).await?;
            println!(
                "  · batch={:>5}M → {:>6.1} ms · {:>6.1} MH/s",
                b / 1_000_000,
                ms,
                mhs
            );
            if mhs > best_mhs {
                best_mhs = mhs;
                best_batch = b;
                best_ms = ms;
            }
        }
        println!(
            "  ✓ batch_size={}M (~{:.0} ms/batch, {:.1} MH/s)",
            best_batch / 1_000_000,
            best_ms,
            best_mhs
        );

        // pipeline_depth 探测: 只在 batch 单次耗时 < target_ms × 0.5 时才有意义
        // (说明 GPU 还有富余, 多 slot 排队能填满)
        let mut chosen_depth: u32 = 1;
        let mut chosen_hashrate = best_mhs;
        if best_ms < target_ms * 0.5 {
            for depth in [2u32, 3, 4, 6, 8] {
                let mhs = self
                    .bench_pipelined(best_batch, depth, warmup_n, measure_n)
                    .await?;
                println!("  · pipeline={} · 实测 {:.1} MH/s", depth, mhs);
                if mhs > chosen_hashrate * 1.05 {
                    chosen_hashrate = mhs;
                    chosen_depth = depth;
                } else {
                    break;
                }
            }
        }

        let _ = best_ms;
        Ok(BenchResult {
            batch_size: best_batch,
            pipeline_depth: chosen_depth,
            hashrate_mhs: chosen_hashrate,
        })
    }
}

fn make_uniforms(challenge: &[u8; 32], difficulty: &[u8; 32], nonce_base: u64) -> Uniforms {
    let challenge_lanes: [u32; 8] = std::array::from_fn(|i| {
        u32::from_le_bytes([
            challenge[i * 4],
            challenge[i * 4 + 1],
            challenge[i * 4 + 2],
            challenge[i * 4 + 3],
        ])
    });
    let difficulty_be: [u32; 8] = std::array::from_fn(|i| {
        u32::from_be_bytes([
            difficulty[i * 4],
            difficulty[i * 4 + 1],
            difficulty[i * 4 + 2],
            difficulty[i * 4 + 3],
        ])
    });
    Uniforms {
        challenge: challenge_lanes,
        difficulty: difficulty_be,
        nonce_base_lo: nonce_base as u32,
        nonce_base_hi: (nonce_base >> 32) as u32,
        _pad0: 0,
        _pad1: 0,
    }
}
