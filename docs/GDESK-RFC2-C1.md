# RFC2 C1 生产 GPU 接线

日期：2026-11-17。工作目录 `/home/xxnuo/projects/rustdesk`。未运行 FFI codegen。

## 结论与范围

scrap 的 GDesk 生产编码器已启用真实硬件 GPU 小波计算，保留原 GDesk 协议、cache、QoS、refinement、ACK/重传状态机。Intel Vulkan 硬件、无适配器 CPU fallback、真实 Xvfb 捕获、活动/静止画面自动设备恢复已通过下述测试。GPU 功能接线完成不代表 4K 性能达标；测量发现明显额外成本，见性能段。

我只改动：

- `libs/gdesk/src/encoder.rs`
- `libs/gdesk/src/gpu/mod.rs`
- `libs/gdesk/src/gpu/production.rs`（新增）
- `libs/gdesk/src/udp.rs`
- `libs/scrap/Cargo.toml`
- `libs/scrap/src/common/gdesk_codec.rs`
- `libs/scrap/tests/gdesk_gpu_capture.rs`（新增）

临时测试脚本、测量源文件及日志均在项目 `.tmp/`。根 Cargo.toml/Cargo.lock 无需修改。没有改 sched.rs、proto、src/、Flutter 或 workflow；这些文件工作树中的并行改动不属于 C1。

## 协议差异与接法

原 `GpuEncoder`/`GDeskVramEncoder` 原型导出 GPU tile/coefficient 或 GDVR 数据包，不能直接交给生产 GDeskDecoder。原型也没有生产 cache/QoS/refinement/ACK 状态机；直接替换会改变协议语义。

新路径将变化且非纯色/非双色、非确定缓存命中的 tile 按最多 64 个一批装入 512×512 RGBA atlas，直接提交既有 `wavelet_pack` 计算管线，读回精确完整系数并转换到生产使用的 Mallat 顺序。原 CPU 路径继续负责完整像素比较、纯色/双色、缓存、熵编码和协议封装；GPU 模式不会调用 CPU rgb_to_wavelet 代算纹理变换。原 CPU 解码器不变。

生产路径不调用原型 sampled scan；其 list 由完整 CPU 比较产生，wavelet list 项显式为非 solid。原型 SCAN_WGSL/shaders.wgsl 未改。新增三色细节测试：红背景仅 `(20,0)` 蓝点和 `(21,0)` 绿点，解码整图精确一致，硬件存在时 dispatch 必须发生。

`GpuContext::new_hardware()` 仅选择 DiscreteGpu/IntegratedGpu/VirtualGpu，按独显、集显、虚拟 GPU 排序，排除 CPU/软件适配器。设备请求用已有 pollster，不创建 Tokio runtime。独立原型 `GpuContext::new()` 保留旧选择路径。scrap 的显式 gdesk feature 同时打开 gdesk/gpu，未打开 gdesk 的路径不受影响。

## 故障恢复

device-lost/uncaptured-error 回调标记故障，首个错误记录日志；生产操作的 GPU Result 错误也标记故障。非阻塞 device.poll 使静止时的回调有机会执行。

生产 `with_recovery` 在故障时丢弃 GPU、reset 原协议状态、递增 canvas_epoch、清空 cache/UDP 状态并重建设备，然后重试同一输入。重建后再次失败则 reset 并明确 CpuFallback。frame_seq 不倒退；旧 ACK 不会确认恢复后不同 seq 的 tile。

静止画面 `poll_refinement` 从已有 last_rgb 恢复紧密 BGRA，重新编码完整画布，不依赖操作系统再次产生脏帧。GPU 故障也会使 has_gdesk_pending_refinement 返回 true。测试直接调用生产 API，没有先手动 recover：可靠/数据报 × 活动 encode/静止 poll 四组均在实际 device.destroy 后自动恢复到 GpuVulkan、epoch+1、MaxError=0。

为直接测试真实 device.destroy，新增 `GDeskEncoder::gpu_device() -> Option<&wgpu::Device>` 只读访问器，无新故障注入状态或 mock 层。

## Overlay 最小接口

- `scrap::gdesk_codec::GDeskOverlayMetrics.pub active_backend: &'static str`。
- 取值：GpuVulkan、GpuDx12、GpuMetal、CpuFallback；旧端/空值/未知值为 Unknown。
- C2 添加的 `EncodedVideoFrame.gdesk_active_backend = 5` 已被发送端普通帧、数据报、refinement、恢复刷新全部填充。
- decoder record 首先设 Unknown，再只用成功解码帧携带的远端字段覆盖，不从本地 decoder、是否安装 GPU 或编译 feature 推断远端后端。
- `GDeskEncoderApi::active_backend()`、`gpu_dispatches()` 可用于生产路径验收；只有 dispatch_workgroups 实际提交才递增 dispatch 计数。
- metrics 无消息时仍为 None；旧消息缺字段为 Unknown。单测显式验证远端 GpuVulkan、CpuFallback、空值和无效值四种情况。

整合提醒：C3 若只传原始 GDesk tile payload，protobuf 的 backend 字段不会随之传输，须通过可靠控制消息携带远端状态并填回 EncodedVideoFrame 后交给现有 decoder。单独的 `handle_gdesk_udp_datagram(raw)` 无 backend 参数，输出 Unknown；不能猜 GPU。此控制面接线由 C2/C3/src 整合方处理，本报告不把独立 AEAD 会话 overlay 宣称为已经验收。

## 给 C3 的 1170 字节预算

`gdesk::udp::GDESK_UDP_PAYLOAD_MTU = 1170`（1200 减 14 字节外头和 16 字节认证 tag）。scrap 生产配置使用此值。

独立 GDeskConfig 默认 udp_mtu=1200，原 `pack_udp_datagrams` 保留原默认；新增 `pack_udp_datagrams_with_mtu`。初始 wavelet 码率阶梯、refinement 单记录适配/分段、最终打包全部使用同一个 udp_mtu；未引入二次压缩或 C3 重包。新预算测试用确定性 xorshift 纹理，必须实际经历 split refinement，断言每包+30<=1200、ACK 收敛和最终像素精确。

## 执行命令与证据

scrap 构建环境使用现有第三方依赖需要的：`CXXFLAGS='-include cstdint' OPENSSL_LIB_DIR=/usr/lib/openssl-3 OPENSSL_INCLUDE_DIR=/usr/include/openssl-3`。未修改第三方库。

| 命令（scrap 命令加上述环境） | 退出码 | 结果 / 日志 |
| --- | --- | --- |
| `cargo test --locked -p gdesk --lib -- --nocapture` | 0 | 24 passed；`.tmp/rfc2-c1-gdesk-lib.log` |
| `cargo test --locked -p scrap --features gdesk --lib -- --nocapture` | 0 | 10 passed，含四组生产自动恢复和 metadata；`.tmp/rfc2-c1-scrap.log` |
| `cargo test --locked -p scrap --features gdesk --test gdesk_phase1_loopback` | 0 | TCP loopback/协商等 2 passed；`.tmp/rfc2-c1-loopback.log` |
| `cargo test --locked -p scrap --features gdesk --test gdesk_gpu_capture --no-run` | 0 | `.tmp/rfc2-c1-capture-build.log` |
| `uv run python .tmp/rfc2-c1-xvfb.py` | 0 | 硬件捕获 1 passed，四组精确；`.tmp/rfc2-c1-capture.log` |
| `VK_DRIVER_FILES=/nonexistent GDESK_EXPECT_BACKEND=CpuFallback uv run python .tmp/rfc2-c1-xvfb.py` | 0 | 无适配器 fallback 1 passed，四组精确且 dispatch=0；`.tmp/rfc2-c1-fallback.log` |
| `cargo test --locked -p gdesk --lib payload_budget` | 0 | 分段 refinement/1170 预算通过；`.tmp/rfc2-c1-mtu.log` |
| `git diff --check -- <上述 C1 已跟踪文件>` | 0 | 无空白错误 |

设备证据：本轮 gdesk 测试直接打印 `adapter=Intel(R) Graphics (RPL-S) backend=Vulkan device_type=IntegratedGpu`，同一日志记录 destroy 后恢复为 GpuVulkan。主调度提供 `.tmp/rfc2/vulkan-summary.log` 独立显示 vendor 0x8086/device 0xa782、Mesa 26.2.7、Integrated GPU。

真实 capture fixture 由 `.tmp/rfc2-c1-xvfb.py` 使用 Xvfb `-displayfd` 分配私有显示，创建 256×128 屏幕：红/蓝区域加逐像素三通道渐变纹理；捕获走真实 scrap Capturer/Frame::to。每次启动和 finally 中关闭私有 Xvfb，不画用户已有 DISPLAY。可靠帧/数据报 × ARGB/ABGR 四组均断言 GpuVulkan、GPU dispatch>0、远端 metadata==backend、所有像素 MaxError=0；数据报额外断言<=1170。无适配器同样四组 MaxError=0，backend=CpuFallback、dispatch=0。

硬件专属 capture test 默认 ignore，运行需要私有 fixture。device_loss 单测无硬件时明确打印 SKIP，不算 GPU 验收；主调度负责 workflow 的 GPU 测试入口。这里的硬件结果未跳过。

早期测试先确认原生产适配器硬件断言失败（exit 101，`.tmp/rfc2-c1-red.log`）；metadata 中间编译曾因 C2 proto 字段未落地退出 101，字段落地后最终已通过。测试过程还修正了测试 ACK 使用旧 seq、ImageFormat 不支持 PartialEq、原纹理未触发分段等测试问题；最终结果以上表为准。

## 性能与内存：已做局部优化，未达标

已按审查提前排除当时确定可用的 cache hit（可靠模式按原配置，UDP 要求 confirmed；unsent 最终若禁止 cache hit，由按需 GPU 计算补齐），不更改缓存生命周期。RGB staging 改为逐批提取/释放，最多 64 tile 约 0.75MiB，避免原先额外保留整帧 RGB 约 23.9MiB（4K）。全帧待消费系数仍可能约 47.8MiB；不能宣称总 CPU 内存固定。

临时 `.tmp/rfc2-c1-4k.rs` 用本轮编译 gdesk rlib 编译到 `.tmp/rfc2-c1-4k`，运行 `.tmp/rfc2-c1-4k gpu` / `cpu`，均 exit 0，日志 `.tmp/rfc2-c1-4k-{gpu,cpu}.log`。同一 3840×2160 渐变重复纹理 A→B→A、强制无损首发，单次开发构建测量：

| 项目 | GpuVulkan | CpuFallback |
| --- | --- | --- |
| A 首帧 encode | 2225.749ms，32 dispatch | 271.889ms |
| B 首帧 encode | 2168.431ms，32 dispatch | 259.025ms |
| 返回已缓存 A | 432.796ms，0 dispatch，2040 cache hit | 218.320ms，2040 cache hit |
| VmHWM / VmRSS | 160952 / 157224 KiB | 57920 / 57848 KiB |

这组纹理首帧在 CPU 顺序处理过程中即有 2028 cache hit，GPU 预计算时这些新槽尚未建立，因此是重复纹理下的额外工作压力，不是随机唯一纹理吞吐或 4K 60fps 基准。未测改动前相同工作负载的 RSS，不虚报优化前后实测差值。跨帧确定 cache hit 已实证 dispatch=0；首次出现的帧内重复仍会预计算。现有生产 GPU 路径在该负载显著慢于 CPU，不能宣称性能已交付；进一步按批消费系数/去重或原生 VRAM 需要另定范围，此处未改架构。

## 最小化检查与回归面

- encoder.rs：仅 GDesk 的可选 GPU 系数来源、预计算、故障恢复 API；gpu=None 时仍调用原 CPU 数学函数。新 udp_mtu 默认沿用 1200。
- gpu/mod.rs：硬件选择/设备故障状态为新生产入口服务；旧原型构造调用 request(false)，不安装生产错误处理回调。没有改旧 GPU 算法。
- production.rs：新 atlas/dispatch/readback 实现；未接 GDVR 原型协议。
- udp.rs：仅替换系数来源和统一可配置预算；ACK、确认槽、重传、分段算法语义未重写。
- scrap Cargo feature：只有 gdesk 开启 GPU 依赖，默认非 GDesk 编码路径不改。
- gdesk_codec.rs：GDesk 构造器/encode/poll/backend metrics 路径变化为本任务必要；其他 codec/公共 trait 签名均未改。
- 新测试只依赖已有框架/原生 Xvfb，无新测试依赖，无 FFI codegen。

## 明确限制

- 这是 CPU 像素上传 + 真 GPU wavelet + 系数读回。input_texture 仍 false，DXGI/DMA-BUF/IOSurface 原生句柄导入和零拷贝不在此次实现内；CPU decoder 保留原路径。
- 根 release profile 仍 `panic='abort'`；catch_unwind 只对允许 unwind 的构建有效，不能保证发布版任何驱动 panic 都可恢复。已验证的是 device-lost/错误回调/Result 路径和开发构建 device.destroy；没有跑 Windows UAC/锁屏、macOS Metal 或发布版故障验收。
- 多 GPU 只尝试排序后的首选硬件；request_device 失败即显式 fallback，不轮询其余硬件。无硬件/重建失败进入 CPU 后本 encoder 不周期探测新设备。
- 未跑完整 Flutter GUI、整仓 cargo test、旧 GPU 原型全部性能门禁或 C3 AEAD 会话 overlay。root 构建及下游 UI 由整合方负责；库/私有 capture 结果不冒充完整会话验收。
- GPU 4K 性能尚未满足生产帧率目标，见实测；无隐藏 CPU 冒充、无 profile 改动掩盖该问题。
