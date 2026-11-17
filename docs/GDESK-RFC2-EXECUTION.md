# GDesk RFC2 执行记录

## 目标与基线

2026-11-17：按 `.tmp/RFC2-GDESK.md` 完成 C1 GPU 生产接线、C2 应用层分片与优先级、C3 同端口独立认证 UDP。基线 `5d46bf48e`。开始时 `.gitignore` 已有修改，保留且排除提交。

## 调度与验收约束

- Carbon high 与 Astra high：独立计划与交叉审查。
- Sol high 与 Argon medium：实现、环境验证与测试。
- 主代理负责范围分配、集成、回归面核对、提交与记录。
- 生产调用、真实抓屏、真实连接验收分别记录；库测试不替代会话验收。
- 非 GDesk 和不支持能力的旧端保持原路径。新协议必须有能力协商与有界输入校验。
- 不运行 Flutter FFI codegen；沿用既有桥接签名。

## 进行中

1. C1 已分配 Sol：`libs/gdesk/`、`libs/scrap/` 及 Cargo 特性接线。要求保持现有协议与 ACK 语义。
2. Carbon/Astra 独立审查 RFC 与现有源码；报告存放 `.tmp/rfc2-*-plan.md`。
3. Argon 核对 C3 实际 socket/密钥路径及真机会话测试条件。
4. 主代理确认 RFC 路径描述存在偏差：当前 KCP 实现在 `src/kcp_stream.rs`，不是 `libs/hbb_common/src/udp.rs`；需以源码确定修改范围，避免不必要的子模块变更。

## 完成判据

C1：生产 GPU 后端可观测，真实抓屏无损，设备错误可恢复。
C2：真实连接大剪贴板分片重组正确，光标/视频持续交错；记录 P99 实测。
C3：同一 UDP socket 旁路独立认证、防重放、可靠流回退；15% 丢包实测与静止画面收敛。
尚未执行的验收不得标为通过。

## 环境与早期检查

- `vulkaninfo --summary`：Intel Graphics (RPL-S)，`INTEGRATED_GPU`，Vulkan 1.4.354；原始输出 `.tmp/rfc2/vulkan-summary.log`。这证明硬件可用，不代表生产 GPU 编码通过。
- 复用前轮私有 Xvfb 脚本 `.tmp/gdesk-audit-xvfb.py` 与真实抓屏程序 `.tmp/gdesk-audit-capture.rs`；不操作用户桌面。
- 下游遥测增加 `gdesk_active_backend` JSON 字段与 Flutter Backend 行，保持现有 FFI 签名；等待 C1 实际编码后端元数据接入。
- C2 分配第二个 Sol，独占 connection/io_loop、bulk 新模块、base 协议、SendScheduler；与 C1 GPU实现分离。
- Flutter 定向 analyze 已执行：16 项诊断，主要错误是缺少 `generated_bridge.dart` 及其类型，另有既有 deprecated/info；日志 `.tmp/rfc2/flutter-analyze.log`。未通过整体分析，不将此记作 GUI 构建成功；当前新增 backend 展示行未被指出错误。
- Flutter `test --no-pub test/quality_monitor_transport_test.dart` 在运行测试前因 `.dart_tool/package_graph.json` 缺失退出，日志 `.tmp/rfc2/flutter-test.log`。未用 mock 或伪造桥接绕过。
- C2 设计：新增能力回告及 bulk 信封；完整 protobuf 经 scheduler 排队，重组大小/并发/期限受限，异常明确失败；详见 `.tmp/rfc2-c2-sol.md`，等待 Astra 复核。
- Argon 开始 C3 KCP分流与独立 AEAD 模块实现；connection/io_loop 由 C2 独占，C3生产集成后续串行进行，避免并发覆盖。

## 集成进展（二）

- 已补齐 `GDeskOverlayMetrics.active_backend` → `QualityStatus` → Flutter JSON → model → Overlay，并扩展既有 Rust/Dart断言；尚待完整构建验证。
- 发现并拦截 C3 草案错误假设：当前 `Encrypt.0` 是发送方向密钥，split key 握手后两端不同，不能当作共同主密钥使用。已要求 Argon 修订并由 Astra 独立复核，未据错误方案启用生产通道。
- C1 正在以真实 GPU wavelet 转换接入既有协议，保留 CPU 的 solid/text 快速记录与 ACK/QoS；需通过非纯色真实捕获验证 GPU 实际执行，不能仅凭后端字符串验收。
- 当前 scheduler 回归已实跑：`cargo test --locked -p gdesk --no-default-features sched::tests -- --nocapture` 退出 0，2 项通过（framed packet 边界/跨消息 pacing、原有严格优先级）。日志 `.tmp/rfc2/scheduler-tests.log`；其余被过滤的 0-test suites 不计为覆盖。仅证明库调度，不替代真实会话 C2 验收。

## 验收证据矩阵（阶段快照，最终结果见文末）

| RFC 条目 | 必须取得的证据 | 当前状态 |
| --- | --- | --- |
| C1 同步初始化 | 生产构造调用独立执行器，无嵌套 Tokio runtime；真实 GPU 构造运行 | 生产适配器验证通过，见C1报告 |
| C1 硬件优先/显式降级 | 硬件选择、软件适配器排除、实际编码后端日志与 overlay 一致 | GPU/fallback与metadata测试通过；完整会话HUD待验收 |
| C1 设备丢失 | 重建/回退后 epoch 增长，ACK 与全屏刷新正确，无断连 | 生产适配器活动/静止恢复通过；完整会话待验收 |
| C1 无损 | Xvfb 实际捕获非纯色图，生产 GPU dispatch 与 Max Error=0 | 实际Capturer四组通过；完整Connection/Remote待验收 |
| C2 优先级与 pacing | 生产所有相关出站路径进入调度，每分片后重查高优先级 | 生产已接，P0/P1门控及credit回归通过 |
| C2 信封与重组 | 双方协商、边界校验、业务处理原路径，旧peer兼容 | v2信用窗口、边界及兼容审阅通过 |
| C2 20MB 真实会话 | Server/Client 实际业务路径，Clipboard完成且持续收到光标/视频，P99记录 | 网络hash/handler/交错通过，P99 106ms vs基线127ms；OS X11读取限制 |
| C3 同端口分流 | 已打洞socket复用，合法KCP不被误吞，TCP回退 | 真实私有rendezvous触发生产KCP/AEAD，可靠回退测试通过 |
| C3 认证与重放 | 方向隔离、序号不回绕、认证后推进64位窗口、外层<=1200B | 密钥修订完成，底层审阅与真实socket测试通过；session待审 |
| C3 15% 丢包 | 真实UDP代理丢包、实际旁路计数、KCP控制计数、tile ACK最终像素收敛 | socket实测15.44%、MaxError0、25.46s；完整生产会话待验收 |
| Flutter | 沿用FFI签名；分析/测试/标准构建结果准确区分 | 沿用签名；本地生成制品缺失导致验证阻断 |
| 最终审查/提交 | Carbon/Astra交叉审查、默认路径回归、每文件必要性说明、分工单提交 | 待执行 |
- 关闭 GPU 特性的库回归：`cargo test --locked -p gdesk --no-default-features --lib` 退出 0，22 项全部通过；日志 `.tmp/rfc2/gdesk-cpu-regression.log`。覆盖既有编码、解码、QoS/refinement 与协议辅助逻辑，不代表整个主工程默认构建通过。
- C3 设计裁定：仅保留一套 seed 协商方案，本端发送会话密钥结合随机熵派生 seed，通过已认证加密流交换后按固定角色顺序派生方向密钥；授权后只协商一次，拒绝重复协商造成序号回绕。避免两套未用密码学实现；交由 Astra 审阅。
- CI补充GPU设备恢复单测入口，使用现有 `run_drm_tests` 防止错误过滤导致零测试假通过。已核对过滤名与实际函数唯一匹配；未宣称无GPU的CI runner能证明硬件恢复成功。
- C2中期审查发现 RawMessage 错误语义可能影响非GDesk会话：原忽略记录错误改为关闭连接。已要求实现者恢复feature-off旧行为，最终diff复核该项。
- 主工程中期编译已实际运行：`CXXFLAGS=-include cstdint OPENSSL_LIB_DIR=/usr/lib/openssl-3 OPENSSL_INCLUDE_DIR=/usr/include/openssl-3 cargo check --locked --lib --features gdesk` 退出101。两处 E0599：connection 调用的 `Transport::has_video` 尚未实现；已反馈C2实施者补齐接口后重测。日志 `.tmp/rfc2/root-check-gdesk.log`。不得引用前轮主工程check成功为本轮结果。
- 主代理独立重跑私有Xvfb：`uv run python .tmp/rfc2-c1-xvfb.py` 退出0。真实256×128抓屏含红/蓝/多色纹理，普通帧与datagram、ARGB与ABGR四组均 `GpuVulkan dispatches=1 MaxError=0`。日志 `.tmp/rfc2/gpu-capture-independent.log`。原DISPLAY未操作，私有Xvfb结束后回收。这是生产编解码适配器硬件验证，不等于完整Server/Client会话或Flutter显示验收。
- Astra阶段报告已交付 `.tmp/rfc2-astra-plan.md`，核心建议转交C2/C3：重组前授权与会话门禁、业务白名单及原权限复用；P2顺序、超时边界；seed方向派生、一次性生命周期与就绪屏障、分流不碰撞、1200B预算。下一阶段审实现与证据，不继续扩大设计研究。
- Carbon阶段报告 `.tmp/rfc2-carbon-plan.md` 已交付。其GPU采样scan误判solid意见经源码复查：原型scan确为部分采样，但最新production实现已直接写入CPU已分类tile列表并绕过该scan；要求局部三色细节回归确认后再判定关闭，不把旧快照发现盲目扩大到原型重写。metadata Unknown断路继续要求修复。
- 对Carbon提出P2保底公平配额、音频改P1等建议暂不采纳：RFC明确严格P0/P1/P2与音频P2，不能借审查改写需求；真实限速测试记录饥饿与带宽边界。ACK现为Misc内字段需按实际分类核对，不按报告假设顶层字段处理。
- C3服务端薄接线已增加：`ConnectionMeta.gdesk_datagram` 仅gdesk特性存在；`udp_nat_listen` 将已打洞 `KcpStream::gdesk_link()` 随连接元数据传下去，不申请新socket/端口。其他连接默认None；Connection字段接收由C2 owner同步。
- 拦截C3实现偏差：seal层增加压缩消息种类来补救内层1200B超过外层预算，不保证不可压缩tile可发送且新增解压边界。裁定移除额外压缩协议，由C1在packetizer生成阶段预留30B认证外头，内层<=1170，外层<=1200；此项正在协调落实。
- 已要求C3新增模块、字段与socket分流全部feature gate，默认关闭GDesk时保持原KCP路径。
- C1后端元数据开始通过 `EncodedVideoFrame.gdesk_active_backend` 传递；对应proto字段由C2唯一owner补入。GPU UDP旁路不携带原protobuf，后续合流必须保留该元数据（包括运行时设备回退后的更新），不能把可靠流HUD通过推及UDP模式。
- 本轮新增既有路径回归面：`server.rs`仅可选ConnectionMeta字段，`rendezvous_mediator.rs`仅KCP成功连接附加现有socket句柄；其他传输仍None。二者是避免重新打洞与把旁路送达真实会话所必需的薄hook。
- Flutter环境复核：`flutter pub get --offline` 退出0，恢复package graph；随后 `flutter test --no-pub test/quality_monitor_transport_test.dart` 退出1，确认仍缺 `generated_bridge.dart`，另有既有类型/依赖错误。日志 `.tmp/rfc2/flutter-{pub-get,test-after-pub}.log`。pub get自动改动的lock及analysis_options已恢复至运行前版本，未引入依赖升级/分析屏蔽；未执行codegen。
- 已验证本轮CI改动的YAML可解析、包含新增恢复测试的容器嵌套run脚本通过 `bash -n`。检查仅证明语法，不代表远端CI已执行。

## 主代理接管传输整合

- C2/C3阶段代码仍有未完成接口与要求，主代理已明确终止两位实施agent（工具确认此前running，随后shutdown）并接管其写集合；不继续并行覆盖共享文件。
- 首次根crate bulk测试构建因C3测试引用不存在的 `EncoderConfig`、`canvas()` 失败，已修为真实 `GDeskConfig`、`canvas_rgb()`；复测日志 `.tmp/rfc2/bulk-integration.log`。
- 主代理已给C3新增模块/字段/通道/分流补gdesk feature gate，默认不创建旁路；删除未授权的压缩数据报分支，直接认证编码器生成的<=1170B负载。相应丢包测试也配置1170B预算。
- 首次20MiB加密socket测试实跑失败：分片边界通过，交错测试30s超时（`.tmp/rfc2/bulk-integration.log`）。测试每发一个信封用Tokio sleep(100us)，实际计时粒度约毫秒，同时生成80KiB/10ms视频，负载逼近/超过该人为服务速率，严格优先级下P2饥饿。保留失败证据，不加违背RFC的P2保底；将明确的50fps（80KiB/20ms）负载另行测量，仍须后续生产限速验收，不能用该降载替代极限负载结论。
- 50fps加密TCP交错测试退出0，2项通过：20MiB剪贴板完整，cursor/video各316，P99=30.550137ms，max=31.802314ms，总测试6.65s；日志 `.tmp/rfc2/bulk-integration-50fps.log`。这是生产bulk helper经真实加密socket测试，尚非实际Remote/Connection会话。此前100fps超时仍列限制。
- UDP初轮根测试5过2失败：seed交换解密错误、丢包后MaxError254。定位为测试用 `send_bytes` 直接发送seed/ACK（该接口不加密），对端按加密流解密导致失败；修为 `send_raw`。原丢包公式实际约12.5%且周期性，改为固定seed PRNG每包15%阈值，保留初轮日志 `.tmp/rfc2/udp-integration.log` 并复测。
- UDP复测：修正加密入口后，6项通过，25轮收敛限制仍失败MaxError170；延长固定上限至200轮并记录轮次（不改编码/ACK算法），真实代理测试4.38s内收敛MaxError0。发送107个数据报、丢9个、KCP104包、最大外层1200B；PRNG每包丢弃概率15%，本次有限样本实际8.4%，不得写成实测恰好15%。日志 `.tmp/rfc2/udp-{integration-encrypted,convergence}.log`。该测试使用真实socket+编解码器+ACK，仍未经过生产Remote/Connection登录会话。
- 主工程默认特性 `cargo check --locked --lib` 已退出0（8.81s），日志 `.tmp/rfc2/default-check.log`，确认C3 feature gate未破坏默认编译。
- Carbon C2最终阶段审查已交付 `.tmp/rfc2-carbon-c2-review.md`。主代理确认并修复bulk窄白名单导致合法截图/长聊天/切换后的非GDesk视频断连；仅新增已存在业务类型，仍拒绝认证/协商/嵌套，重组后保留原业务权限入口。新增一个表驱动真实消息往返回归；CloseReason即时发送和生产仲裁问题交session owner同步修复。
- 扩大UDP真实socket样本至512×512：测试退出0，24.04s收敛，772个独立认证视频数据报中丢115个（14.9%），KCP控制322包，最大外层1200B，最终MaxError0。日志 `.tmp/rfc2/udp-convergence-512.log`。此前256样本仅8.4%的覆盖不足已由此补强；24s收敛耗时明确保留，不能宣称数个RTT收敛。该测试使用模拟capture时间/RTO，已请求诊断与真实时序差异，尚非完整生产登录会话。
- 大聊天、截图、切换编码器回归测试退出0：`.tmp/rfc2/bulk-business-regression.log`。原授权与业务handler仍承担操作权限，分片层仅恢复合法业务可达性。
- C3分流边界修正：magic识别与合法长度验证分开，带完整GDesk保留头但过短/过长的包在UDP泵直接丢弃，不能因长度错误回落到KCP输入。新增畸形长度断言，`cargo test --locked --lib --features gdesk kcp_stream::gdesk_datagram::tests -- --nocapture` 三项通过，日志 `.tmp/rfc2/datagram-boundary-tests.log`。
- Astra开始C3底层代码复核；独立Sol检查实际Remote/Connection会话的可运行入口与隔离测试环境，输出`.tmp/rfc2-session-e2e/report.md`。生产登录会话验收仍未完成，现有helper/socket结果不能替代。
- 新调度接口 `next_ready_at_with_bulk` 与 `pop_ready` 使用一致的P2可发送条件：上游仍待分类时不阻塞已入队P0/P1，也不让禁发P2导致pacing窗口中持续零等待轮询。三项scheduler回归通过（`.tmp/rfc2/scheduler-upstream-tests.log`）；生产loop调用由session owner接入。
- C1实现者只读分析确认原24s测试使用10ms RTO、16tile预算与递增伪时间，并非生产默认。主代理已将测试改为真实monotonic时钟、默认160ms RTO、scrap生产128tile预算复测，保留原结果不替代新测量。
- 生产参数时钟的UDP复测退出0：829包丢128（15.44%），KCP292，max_udp=1200，MaxError0，最后画面变化至无损并全部确认25.199s，resent_tiles=129。日志 `.tmp/rfc2/udp-production-timing.log`。说明先前24s不能仅归因于伪时间，当前开发构建/loopback代理/顺序收ACK测试仍有显著收敛成本；不得写成低延迟已达标。完整生产loop仍需实测。

## C1 独立提交

- 已提交 `de30b3a22 feat(gdesk): connect hardware GPU encoding and recover device loss`，提交前scrap lib 10项重新执行通过（`.tmp/rfc2/c1-precommit-tests.log`）。暂存范围仅C1文件、UI遥测、CI恢复入口以及proto的EncodedVideoFrame backend单字段；C2/C3协议与代码、原有.gitignore改动均未混入。
- C1详细报告保存至受版本控制的 `docs/GDESK-RFC2-C1.md`，包括回归面、硬件与fallback/恢复证据、4K实测性能限制、发布版panic策略及尚未验收的GUI/平台边界。此提交不是整体目标完成声明。

## 会话接线与第二轮交叉审阅

- 默认 KCP 两项真实loopback回归通过：`.tmp/rfc2/default-kcp-tests.log`，包含往返关闭及关闭前完整帧交付。
- Astra底层C3审阅 `.tmp/rfc2-astra-c3-review.md` 未发现阻断漏洞；确认完整头nonce绑定、双向派生、共享序号与认证后重放窗口、有界demux。该结论明确不覆盖尚在整合的session权限/ready/fallback。
- 按审阅指出的证据边界，丢包测试的两端seed由同进程直接传参改为经加密KCP `send_raw`/`next_timeout`交换；待最终重跑。仍仅对服务端GDesk数据报注入丢包，不能据此宣称所有KCP控制包也遭15%丢包。
- 第二轮根KCP测试和隔离会话启动器编译均遇到session代码引用尚未加入proto的 `active_backend` / `display` 字段，日志 `.tmp/rfc2/kcp-final-tests.log` 与 `.tmp/rfc2-session-e2e/build.log`。已交session实施者补齐，当前不将此轮标为编译通过。
- 真正双端验收路线已具体化：隔离网络/进程与私有Xvfb，临时启动器include原生产源码，运行 `Connection::start` 与 `Remote::io_loop`，使用现有CM IPC授权/RawMessage注入与UI观察trait采集解码结果。尚未成功运行，详见 `.tmp/rfc2-session-e2e/report.md`；观察trait不替换生产抓屏/编解码/传输，但也不代表Flutter GUI验收。

- 协议字段补齐后，根KCP测试重新编译通过并4项全过（`.tmp/rfc2/kcp-final-tests.log`）。本轮seed确经加密流交换；829个GDesk包丢128（15.44%），KCP294包，max_udp1200B，MaxError0，25.457s，重传tile129。仍为真实socket测试，不是完整生产登录证据。
- 隔离生产启动器已编译通过，首轮实际运行进入Connection但未完成画面/Clipboard验收：Clipboard0B、视频0、bulk0，`.tmp/rfc2-session-e2e/run.log`。服务端隔离环境生成ID失败等日志由实施者继续定位，不能把两端进程存活视为通过。
- 为补C3生产登录证据，复用C1 Sol另负责 `.tmp/rfc2-udp-e2e/`，探索通过私有rendezvous现有协议进入Client::start的KCP分支；不修改生产连接入口，不与TCP验收目录交叉写入。

- 根 `cargo test --locked --lib --features gdesk gdesk -- --nocapture` 21项全部通过（`.tmp/rfc2/root-gdesk-final-tests.log`），包含新增session ready/metadata/fallback实际加密KCP测试、bulk边界/兼容业务、原输入唤醒/ACK与UDP丢包。整体并行测试下20MiB helper/socket样本cursor/video各307，P99=28.941ms、max49.026ms；UDP收敛25.388s。该次max高于先前独立测试，保留实际数值，不宣称无长尾。

## C2/C3 当前回归面（最终提交前仍需复核）

| 既有文件 | 改变的运行路径与必要原因 |
| --- | --- |
| libs/base/protos/message.proto | 新peer能力与完整分片信封、一次性UDP协商/后端metadata；消息编号追加，旧字段不改 |
| libs/gdesk/src/sched.rs | 新完整protobuf帧入队与pacing入口，避免切碎加密帧；禁发P2时仍让已分类P0/P1前进 |
| src/server/connection.rs | 仅gdesk特性下协商、出站调度、分片重组、UDP视频发送；feature-off保留既有发送错误/关闭与业务handler |
| src/client/io_loop.rs | 仅gdesk特性下客户端调度、分片合流与认证UDP视频入口；合流复用现有解码/ACK处理 |
| src/kcp_stream.rs | 仅gdesk特性复用已打洞socket并分流到有界旁路；关闭特性不创建新队列或额外分流 |
| src/rendezvous_mediator.rs | KCP建立后向ConnectionMeta附带原socket link；其他连接None，避免二次打洞 |
| src/server.rs | ConnectionMeta新增feature-gated可选link，将KCP能力传入生产Connection |
| src/lib.rs | 注册feature-gated gdesk_bulk/session模块，无默认运行行为变化 |

新逻辑集中在 `gdesk_bulk.rs`、`gdesk_datagram.rs`、`gdesk_session.rs`。未改hbb_common子模块、未执行FFI codegen、未接触用户daemon或桌面。`.gitignore`的既有修改不纳入提交。

- 主代理额外穿透发现UDP视频分支可能先于已入scheduler的P0发送：`send_gdesk_p0`实际只enqueue，随机select的UDP分支没有重新检查。已交实现者与Carbon复核最小修复；不得仅凭上面的21项通过就判定严格优先级完成。

- 最新默认根crate check通过（`.tmp/rfc2/default-session-check.log`，6.71s）。
- Astra最终session审阅发现一处真实metadata漏接：C2未激活时旧视频入队可进入新UDP发送，但绕过prepare_video。已要求公共出队入口补准备/回退，避免仅修一个入队点；报告 `.tmp/rfc2-astra-session-review.md`。
- C3明确限制：Ready通过可靠流协商，不证明独立数据报可达。选择性黑洞GDesk而保持KCP畅通不会触发发送错误回退，不能宣称黑洞自动恢复；RFC要求的无UDP/TCP路径仍回落可靠流。可靠metadata与UDP无跨通道到达顺序保证，可能短暂Unknown/旧backend；不为展示一致性新增epoch/ACK状态机。

- 隔离双端已实际完成CM授权与PeerInfo，真实客户端持续调用解码观察端，QualityStatus报告GDesk/GpuVulkan；本轮终于覆盖实际Connection/Remote路径，尚未取得最终MaxError/交错汇总，不提前判定完成。
- 系统剪贴板读取触发arboard `Maximum request length exceeded`，xclip10s超时。实施者将分别保存客户端网络完整重组/业务入口证据与OS clipboard限制，不改无关依赖、不将网络成功等同系统剪贴板成功。
- UDP P0越队与metadata漏接已进入局部修复：公共UDP发送前发送已有P0、执行prepare_video，后者涵盖C2未协商的旧入队路径。必要接口交由同一session owner修改bulk/scheduler，主代理暂不并发写入。

- 最小化复审后，P0修复改用Carbon提出的两处只读门控：select前有P0则不激活UDP定时分支，真正pop视频前再次检查P0；复用已有scheduler出队，不增加专用send_control/pop_control或重复drain。删除刚引入的重复出口与只服务它的helper；相应回归改为经现有send_one验证真实加密TCP收包顺序。P1 actual-priority backlog修复保留。
- Carbon余项按可达性裁定：64MiB超限保持有界clean failure，不引入生命周期/非致死丢弃机制；pending视频非空时可靠承载合法datagram不是丢帧，不为假想未来重入删除防护。TerminalAction在单独非remote会话中不能激活bulk，要求审阅者给实际可达路径后再决定白名单扩展。
- 真正Connection/Remote TCP会话取得结果（`.tmp/rfc2-session-e2e/run.log`）：20MiB Clipboard wire重组SHA256与源一致，bulk窗口6.614s，交错video43/cursor54/ACK44，GpuVulkan，最终131072字节RGBA的RGB MaxError0；wire parse errors0。OS剪贴板读取失败仍保留。P99/max帧间隔251.179ms，尚未满足无长尾结论，正补同场景无Clipboard基线。该会话TCP为未加密直连，密码授权真实执行；加密协议另有真实socket测试，不能混写为该会话已加密。

- 最小化后根GDesk定向全套重新执行22项全部通过（`.tmp/rfc2/root-gdesk-minimized-tests.log`，26.46s）。新增真实加密TCP回归验证P0完整重组先于P1/P2、send_one返回实际priority。20MiB helper样本video/cursor各330，P99=32.839ms、max36.518ms；UDP helper15.44% loss，MaxError0，26.454s。与先前21项的代码快照分开记录。
- UDP完整会话启动器已编译且运行到私有KCP尝试，但首次结果未产生解码像素（`.tmp/rfc2-udp-e2e/run.log`），仍在定位登录/协商；不以helper的无损结果替代该失败。

- 生产UDP入口最新进展：私有rendezvous驱动真实Remote登录，客户端日志 `secured=true direct=true type=UDP`，收到实际PeerInfo并持续解码，后端GpuVulkan。代理初段确认4个独立AEAD包和正常KCP PUSH/ACK，解析错误0（`.tmp/rfc2-udp-e2e/run.log`）。这不是最终15%丢包结论，后续同入口仍在运行收敛测量。

- 最新默认未用字段最小化：删除Connection中与Session重复且从未读取的gdesk_datagram link副本，以及bulk未用next_ready_at wrapper；所有调用仍走with_bulk版本。根feature gdesk check通过（`.tmp/rfc2/final-gdesk-check.log`，7.21s）。
- TCP无Clipboard基线已实跑：同6.614s窗口，video/cursor/ACK各109，P99=101.980ms、max105.012ms、MaxError0；与bulk样本P99=251.179ms相比确有明显增加，C2“无长尾”尚未通过，继续分析真实wire与代理限速行为，不能凭持续交错就关闭工单。证据`.tmp/rfc2-session-e2e/tcp-baseline/summary.json`。
- 生产UDP首次无损收敛验收失败：即使0%丢包，GpuVulkan且secured UDP已成立，518个AEAD、最终MaxError203（`.tmp/rfc2-udp-e2e/summary.json`）；当前不能把它误写为15%丢包通过。已要求保留失败证据并定位fixture或真实ACK/refinement断点，不能仅延长等待。

## 实会话失败定位与局部修复

- TCP最差251ms gap前实际排了49个P2片、总832167B，4MiB/s理论串行约198ms。说明应用层16KiB优先级仍不能抢占已进入TCP内核缓冲的后台积压；不是单个分片造成。Astra评估最小有界inflight方案，TCP验收者先修正代理逐包sleep/同线程绘图的计时偏差，同带宽同负载比较。
- UDP逐包累计ACK复现成功：既有MTU回归扩大至512×64，使初始帧有两个数据报；按真实客户端每packetACK后，完整帧已确认但再次提交同图却产生重发，red退出101（`.tmp/rfc2/udp-partial-ack-red.log`）。根因ACK0先置unsent，随后ACK1只置confirmed，遗漏清除unsent。不是推测性的状态重构。
- 最小修复仅在同seq有效ACK1时 `unsent[index]=false`；已检查全部读写点：encode与poll均以unsent决定重发，encode_tile原已清除，RTO仅未确认时置位。迟到/错误seq仍由原检查拒绝，确认后的旧ACK0仍被忽略。扩展原测试保持MTU/split/无损断言，并增加确认后同图无重发、收敛后poll空。green退出0（`.tmp/rfc2/udp-partial-ack-green.log`，1项通过）。这改变GDesk UDP ACK路径，真实生产收敛尚待重建复测。

- ACK局部修复库24项通过，Sol只读复核认可其同seq/confirmed门禁与所有unsent读写，报告`.tmp/rfc2-ack-review.md`。完整UDP复测仍失败MaxError194，XGetImage源回读MaxError0，KCP双向duplicate PUSH均0；因此该局部bug不是全部根因。当前临时增加仅trace级seq/ACK/RTO诊断，最终必须移除，不能凭库通过关闭C3。
- TCP恒速累计字节时钟测量（同4MiB/s/原负载）减少代理误差后，Clipboard窗口5.178s，仍43video/55cursor/44ACK，P99=203.247ms，20MiB hash及MaxError0。说明旧251ms含测量开销，但长尾仍存在；新baseline尚在执行，旧证据独立保留。

- 真实trace进一步确证RTO起点错误：seq45细化编码329036us，紧接poll将所有tile以age332150us判RTO（默认160000us），seq46低质量INIT重置链；随后seq45从bitmap1递增到255的ACK全因sent_seq46被忽略。零丢包也不断重复，解释生产与快时钟helper结果分歧。
- 第二处局部修复：finish_udp完成编码/打包后，对本次record更新已有sent_ts为capture_ts+实际编码耗时，RTO从可交付发送时开始，不把编码CPU/GPU成本算网络超时。不改RTO常量、不引入新状态。API级回归encode后按header延时立即poll：修前错误重发INIT退出101（`.tmp/rfc2/udp-rto-red.log`）；修后25项库测试全过（`.tmp/rfc2/udp-rto-green.log`）。真实会话重建复测中，暂留trace供核对、最终移除。

- 修复RTO后真实零丢包UDP会话通过：3.888s收敛、MaxError0、GpuVulkan、secured UDP、尾部3秒AEAD0、KCP双向duplicate PUSH0。15%逐包概率丢弃配置实测168包丢22（13.10%），10.711s收敛、MaxError0、尾部3秒AEAD0、KCP双向duplicate PUSH0，最大外层1200B；丢包仅s2c GDesk AEAD，KCP未主动丢弃。证据 `.tmp/rfc2-udp-e2e/summary.json` 及即将归档报告。不是全链路所有UDP均丢15%。
- 临时三处GDesk audit trace已全部移除；去trace后的25项库回归通过（`.tmp/rfc2/udp-final-regression.log`）。Sol补充复核认可RTO起点修复，报告`.tmp/rfc2-ack-review.md`，最终实会话去trace构建待复核。
- C2按真实49/51片积压修复在途背压：bulk协议版本2、P2全部使用16KiB内信封（含小消息）、每发送片分配单调credit_seq，最多4片未确认；接收验证/重组后直发控制ACK，恢复信用。发送端只有sent/acked两计数，无定时器、重传缓存或配置项；无信用时next_ready/pop条件一致，P0/P1照常前进。双方原授权remote门禁同时适用于BulkChunk/Ack；超前ACK拒绝，旧ACK幂等。
- 新版信封单片允许小业务消息，仍禁止嵌套bulk/ack/transport，超大消息仍走原业务白名单；重组后继续现有业务权限路径。未协商版本2的peer继续原完整消息路径，不能误发对方不支持的credit信封。
- 新credit边界、P0可前进、超前ACK拒绝与20MiB真实加密socket等5项回归通过（`.tmp/rfc2/bulk-credit-tests.log`），784video/cursor，P99 31.953ms/max33.647ms，总16.52s；完整Connection/Remote恒速同负载重新编译测量中，尚不以helper通过关闭C2。

- 完整主工程含credit后的GDesk回归23项全部通过（`.tmp/rfc2/root-credit-final-tests.log`）。另补严格P1检查：已有UDP视频定时到期时，同步禁止scheduler的P2 ready与pop，让UDP P1先发；不新建状态、不影响未到期窗口内P2。
- credit v2真实TCP首轮（`.tmp/rfc2-session-e2e/credit-with-clipboard/summary.json`）：同4MiB/s/原图案/32帧代理队列，20MiB1281片hash匹配，5.371s窗口，video91/cursor90/ACK93，P99=max105.808ms，GpuVulkan、MaxError0、parse errors0。较恒速旧实现203.247ms改善约48%；同新版baseline正在运行，不能跨二进制比较后直接宣称最终通过。OS X11剪贴板限制仍存在。
- 为完整交付复现证据，另由Sol仅整理 `tests/gdesk_session_e2e/` 脚本和README；真实生产include、CM授权、私有rendezvous与bwrap隔离沿用已实测方案。只保存可复现源，不提交临时密钥、配置、20MiB文本、日志或target，不增加生产mock/测试hook。最终需从整理后入口验证。

- 新版成对TCP基线完成：同二进制、4MiB/s、5.371s窗口，baseline P99/max126.868ms、video91/cursor86/ACK91；bulk P99/max105.808ms、video91/cursor90/ACK93。此受控样本未出现bulk额外长尾，20MiB完整hash、GpuVulkan与MaxError0均保留。不能推广到任意带宽、极限负载或WAN RTT。证据`.tmp/rfc2-session-e2e/credit-{with-clipboard,baseline}/summary.json`。
- UDP两处根因修复已独立提交 `be96e530218d18b8784bbab3ce2cbbcfb1e97f7c fix(gdesk): preserve confirmed tiles and start UDP RTO after encoding`，仅udp.rs的必要生产修复与2项相关回归，不包含临时trace/C2/C3未提交代码。
- 默认feature在credit新版协议后根check通过（`.tmp/rfc2/default-credit-final-check.log`，10.31s）。当前完整transport代码仍待最后cross-review/复现脚本和去trace真实UDP复核后提交。

- 后台credit当前实现已获两份Astra限定静态审阅通过（`.tmp/rfc2-c2-backpressure-plan.md`、`.tmp/rfc2-credit-astra-final.md`）。真实配对证据已提供审阅者，4片是payload信用上界，不是任意网络延迟上界。
- 汇总报告保存为 `docs/GDESK-RFC2-TRANSPORT.md`，保留OS剪贴板、固定窗口吞吐、UDP黑洞、metadata乱序及GUI/平台限制，未将部分验收扩大为完整GUI验收。

## 提交版复现入口的独立验收

- 主代理从 `tests/gdesk_session_e2e/` 的实际脚本独立prepare/build/run（不沿用旧临时binary）：两个build均exit0，构建前后源码摘要一致；日志`.tmp/rfc2/repro-{credit,udp}-build.log`。独立target以项目现有缓存的CoW副本加速，所有产物仍在项目.tmp。
- TCP负载/基线均exit0：20MiB1281片hash一致、MaxError0、GpuVulkan；负载P99/max133.859ms、baseline126.509ms，增加7.350ms。直接分析代理入口read_t的credit：sent=acked=1282、峰值4、全部归还，`.tmp/rfc2/repro-credit-window.json`。出口时间可能已消耗部分信用，不能将出口观测峰值更小误判成实现没有四片窗口。
- 为落实RFC的断言要求，baseline入口增加明确本场景gate：负载P99不超过基线P99加64KiB在4MiB/s下串行时间15.625ms。新入口再运行exit0（`.tmp/rfc2/repro-credit-gate.log`），负载133.859ms、此次baseline138.351ms。保留第一次baseline126.509ms，未挑选更差基线掩盖失败：两次均满足同一预算。预算是回归场景阈值，不是WAN硬实时保证。
- 提交版UDP入口15%概率轮exit0（`.tmp/rfc2/repro-udp-loss15.log`）：168包丢22（13.095%），10.790s收敛MaxError0，原图回读0、GpuVulkan、secured UDP、最大1200B、KCP双向重复PUSH0、尾部3秒AEAD0；全部私有子进程已回收。
- 最新生产代码去trace+credit v2+P1/P2门控的独立UDP owner轮也exit0：`loss15-credit-final/`，10.675s、MaxError0，16项构建前/后/当前源摘要一致。早前`loss15-final/`虽去trace但尚不含credit，其版本边界已在报告更正。
- 提交前回归面与范围审查：每个既有文件只保留C2/C3协议、调度、会话接线必要变化；未触碰hbb_common、未修改既有非GDesk业务逻辑，未stage原.gitignore。临时trace已清空。完整复现源与README入库，临时密钥/配置/画面/日志/target不入库。

- 最终交叉审阅收口：Carbon报告`.tmp/rfc2-carbon-credit-review.md`通过，确认UDP到期P1/P2门控一致、credit授权接线与TerminalAction范围；Astra报告`.tmp/rfc2-credit-astra-final.md`通过、0项发现。Carbon未重复审阅credit内部算术，其范围由Astra专项覆盖。复现脚本owner已停止修改，主代理核对暂存范围仅含计划文件，既有.gitignore保持未暂存。C1、C2、C3按本报告限定环境完成；平台GUI、OS剪贴板及性能限制保留，不扩大验收结论。
