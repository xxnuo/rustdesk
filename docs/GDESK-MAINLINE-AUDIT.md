# GDesk 主链路穿透审计

审计日期：2026-11-17。基线：`c6fff92e0`。本报告依据生产调用路径检查；库级门禁和合成帧测试不作为真实 RustDesk 会话已验证的证明。

## 结论

此前“Phase 0–5 全部完成”的结论不能代表生产主链路完成。当前可用路径是可选 `gdesk` feature 下的 CPU 像素缓冲编码与解码；仍有独立原型和尚未实现的传输能力。Windows/macOS GUI 实测尚未进行。

## 已确认未接入的能力

1. `libs/scrap/src/common/gdesk_codec.rs` 的 `GDeskEncoderApi` 持有 CPU `GDeskEncoder`，`input_texture()` 返回 `false`；生产 `src/`、`libs/scrap/` 没有调用 `GDeskVramEncoder` 或 `GpuEncoder`。GPU 门禁耗时不能作为生产会话编解码耗时。
2. `libs/gdesk/src/gpu/vram.rs::register` 对 DXGI、DMA-BUF、IOSurface 原生句柄明确返回 `native handle import is not available in this build`，不是“已经实现但尚未真机测试”。独立 GDVR 帧格式亦不能直接替换生产 CPU 协议。
3. `src/server/connection.rs` 中数据报最终仍通过 `conn.stream.send` 发送。1200 字节分包与 ACK 算法已经存在，但没有独立不可靠 UDP 副通道，不能消除可靠流的队头阻塞。
4. `libs/gdesk/src/sched.rs::SendScheduler` 没有生产调用。真实连接采用不同 channel 和直接 stream 写入；库里的 16KB P2 分片及排队延迟门禁不证明生产剪贴板/文件传输满足同样保证。给现有消息任意切片会破坏接收协议，不能用简单替换修复。

上述项目需要完整的平台导入或传输协议实现，不能在审计中用测试通过或声明接口来标记闭环。

## 本次接线修复与验证

- 光标生产者经 `Subscriber::send` 直接进入 P0；保留跟随光标切屏和 macOS Retina 转换，限制单轮 drain，避免无限占用连接循环。
- GDesk 的 notifier 等待可感知输入 wake；保留帧确认背压，输入到来后省去确认后的剩余帧间等待。1.5ms 是延迟通知参数，并非实际抓屏延迟保证；未确认帧、多显示共享 wake 和 stream 阻塞仍有限制。
- 补齐 Flutter+GDesk 测试中 VideoThread 的 metrics 初始化。
- `build.py --flutter --gdesk` 现在能沿原打包流程开启 feature；默认构建不变。
- 在现有 `session_alternative_codecs` JSON 返回值中暴露 GDesk 能力，Flutter 会话工具栏仅在支持时显示 GDesk。沿用既有 preference 写入与协商；Auto 策略不变。
- 增加 CI 的 `flutter,gdesk` 测试组合，不改变 FFI 函数签名，不执行 codegen。

## 主链路证据

| 环节 | 生产调用与结论 |
| --- | --- |
| 激活 | 根 Cargo `gdesk` → scrap/gdesk；原来标准打包入口及工具栏缺入口，本次补齐。双方支持且显式选择才能协商 GDesk |
| 抓屏编码 | video_service 的 `frame.to(encoder.yuvfmt())` → GDeskEncoderApi::encode_to_message；实际为 CPU BGRA 路径 |
| 发送 | Subscriber 视频队列 → connection select → stream.send；光标 P0 已修复，但严格 P1/P2 及统一 P2 分片仍未实现 |
| 接收解码 | io_loop 接收 VideoFrame → start_video_thread → VideoHandler → scrap Decoder；成功解码/回调后生成 gdesk_decoded_frame_ack |
| ACK闭环 | ACK 经 session Data::Message 发回 → connection GDeskAck → VideoQoS::user_gdesk_ack/queue_gdesk_tile_ack → video_service take_gdesk_tile_acks/apply_gdesk_tile_acks 和 set_gdesk_qos |
| 输入 | io_loop 写租约并发送冗余输入 → connection 验证权限/guard → 输入线程；input_service 实际修饰键纠正路径存在。wake 通知已有调用，但不是硬实时端到端保证 |

## 像素格式

源码核对与真实 X11 捕获一致：Windows DXGI/macOS Quartz 为 BGRA；Wayland、Android 等可能为 RGBA，`Frame::to` 按源格式与 stride 转为编码器要求的紧密 BGRA。解码 `ImageFormat::ARGB` 输出 BGRA，`ABGR` 输出 RGBA；这些是 libyuv 命名，不能按字面解读字节顺序。

客户端 Flutter Windows/Linux 使用 ABGR/RGBA，macOS 使用 ARGB/BGRA。检查本地锁定的 texture_rgba_renderer 插件：Linux/Windows直接接 RGBA pixel buffer；macOS 建立 `kCVPixelFormatType_32BGRA`。没有发现需要修复的红蓝交换。

已在私有 Xvfb（不修改用户桌面）绘制红蓝色块，用真实 scrap Capturer 捕获 128×64 BGRA、stride 512。经真实转换/编码/解码 API，普通帧和 datagram 各验证 ARGB/ABGR 输出，四组全部 8192 像素精确一致。未调用 Flutter 插件屏幕回读，不等于 Windows/macOS 真机显示验收。

## Flutter 构建

GDesk 遥测沿用 update_quality_status JSON 事件；菜单能力同样沿用返回 String 的既有 FFI。无新增签名或类型，无需为本次修改另行手工生成桥接。

但干净 checkout 的标准构建仍依赖原项目生成桥接制品和预编译 Rust 库：generated_bridge.dart、bridge_generated.rs 被 gitignore，CI 原有 bridge 工作流生成并恢复它们。本地缺这些文件，因此不能声称完整 Flutter 构建通过。当前 Flutter 测试在加载阶段失败，还出现已有依赖/类型错误；没有运行 codegen 或伪造生成文件掩盖问题。

## 验证结果与回归面

- scrap GDesk 单测 6 项、TCP loopback/协商测试 2 项、root GDesk 测试 12 项通过；这些是补充回归检查，不替代会话实测。
- root `cargo check --locked --lib --features gdesk` 及默认 feature 检查均通过。构建使用本机现有 OpenSSL 3 路径及 `CXXFLAGS=-include cstdint` 解决旧第三方库编译环境问题，未修改依赖。
- Flutter overlay/质量测试文件定向 analyze 无问题；toolbar analyze 仅报告原第648行 info。Flutter测试未通过，完整GUI/新增CI组合未验收。
- build.py 五组参数输出、CI YAML/内嵌shell语法检查通过。
- `video_service.rs` 仅改变 GDesk 等待；非 GDesk 仍用旧路径。`connection.rs` 改 GDesk 光标路由/P0发送，保留权限和转换。`flutter_ffi.rs`、toolbar 增加可选能力入口，旧peer不显示。build.py仅显式开关生效；CI增加检查。`io_loop.rs`、`client.rs`、gdesk_codec.rs仅新增/修复测试，不改对应生产行为。

## 验收状态

本轮修复了可局部闭环的接线错误，**严格主链路整体审计尚不通过**：原生 GPU 句柄、实际 GPU 编码、独立 UDP、严格 P1/P2 与真实后台分片仍缺实现；端到端会话及 Windows/macOS GUI 未测试。不能继续引用早先独立门禁指标作为生产完成证据。

详细当轮日志与分项分析保留于 `.tmp/gdesk-audit-{chain,pixels,flutter}.md`、`.tmp/gdesk-audit-*.log` 和 `.tmp/flutter-gdesk-audit/`；本报告受版本控制，明确结论不依赖临时日志长期存在。
