# RFC2 C2/C3 传输闭环

本报告与 [执行记录](GDESK-RFC2-EXECUTION.md)、[C1 GPU 报告](GDESK-RFC2-C1.md)共同记录实现、失败、修复和验证。未运行 Flutter FFI codegen。

## 实现

- C2：真实 Connection/Remote 使用 SendScheduler；P0 优先，P1 保留64KiB/4ms pacing；UDP 视频到期时同样阻止P2抢先。每次发送前重查生产队列。
- 大业务消息以16KiB payload的完整protobuf信封传输，逐帧加密；接收有单消息64MiB、总预算128MiB、并发4条及30秒空闲超时约束。重组后复用原业务权限与handler。
- 真实测试发现仅分片仍会让TCP预排队约0.85MiB后台数据，故bulk v2加入每方向4片累计信用窗口。所有P2（包括小消息/压缩文件块）计费；无信用只阻止P2，不阻止P0/P1或读取ACK。准确协商v2才启用，旧端走原可靠消息路径。
- C3：复用已打洞的KCP socket和唯一收包泵，旁路有界256包，不另开端口。魔数还校验KCP保留字节，避免conv碰撞。
- 授权加密流内交换随机seed，固定角色顺序派生双向独立key；XSalsa20-Poly1305逐包认证，完整头绑定nonce，共享单调64位序号、认证后更新64包重放窗口，外层不超过1200B。
- 双向Ready后才发送旁路视频，客户端重建VideoFrame进入原解码/ACK路径。TCP、未协商、无UDP及显式发送错误回退可靠流。
- 静止无损验收暴露并修复两处UDP状态错误：完整ACK清除先前部分ACK留下的unsent；重传计时从编码完成开始，避免本地329ms编码超过160ms RTO后立即重置细化链。独立提交 `be96e5302`。

## 真实会话证据

临时/可复现启动器原样include生产源码，观察端只实现已有UI回调。服务端真实Connection::start，客户端真实Remote::io_loop，私有Xvfb真实scrap抓屏和GpuVulkan编码/真实解码。没有替换主循环或mock队列。

TCP通过现有CM IPC授权并注入20MiB Clipboard；UDP通过私有rendezvous实现现有控制协议，触发原生产打洞/KCP路径及真实身份验证。所有进程处于私有网络、进程、IPC和挂载命名空间，HOME不重设，用户daemon/桌面未操作。

| TCP同4MiB/s、动态画面、同版对照 | 有20MiB Clipboard | 无Clipboard基线 |
| --- | ---: | ---: |
| 观察窗口 | 5.371s | 5.371s |
| 视频包 | 91 | 91 |
| 光标包 | 90 | 86 |
| GDesk ACK | 93 | 91 |
| 视频P99/max间隔 | 105.808ms | 126.868ms |
| 最终RGB MaxError | 0 | 0 |

20MiB共1281片，最大payload16384B，重组SHA256与源一致，客户端原handler记录更新。修复前公平恒速测试P99为203.247ms，对应基线116.943ms；新增信用窗口消除了本样本中的额外长尾。旧逐包sleep测量得到251ms，保留为历史，不混入最终对照。

UDP去trace实会话：0%丢包3.896s收敛；15%概率设置下168包丢22（实际13.095%），10.711s收敛。两轮MaxError0、GpuVulkan、secured UDP、外层最大1200B，收敛后3秒AEAD0，KCP双向重复PUSH均0。仅对s2c GDesk数据报丢弃，KCP控制包未主动丢弃。当前版本及提交版脚本复核也通过：10.675s与10.790s，详见执行记录。

提交版脚本独立复跑TCP为133.859ms对基线126.509ms，仍在64KiB窗口的15.625ms串行预算内；已把此成对断言加入baseline入口并实跑通过。两次对照保留在执行记录，未用单次结果承诺硬实时。

## 已完成检查与范围

根GDesk定向测试23项、GDesk库25项及默认feature根check通过。Astra对密码底层、session权限与Ready、credit和兼容路径复核未发现未修阻断；Carbon发现的关闭通知、生产UDP旁路、P0越队、上游空转及准确P1 backlog已修正。所有阶段日志、源码快照与失败证据在执行记录中标明。

新增逻辑位于gdesk_bulk/datagram/session模块；既有connection/io_loop必须接入生产循环，kcp_stream必须复用原socket，rendezvous_mediator/server只传递可选link，lib仅注册feature-gated模块。proto新增字段，sched新增完整帧调度。feature关闭时不创建旁路、不走新业务发送路径。没有修改hbb_common子模块或共享stream trait。

## 限制

- 20MiB网络重组及业务接收通过，但当前arboard/X11读取系统剪贴板报Maximum request length exceeded；没有修补该独立依赖问题，不宣称OS剪贴板可粘贴成功。
- 64KiB指未确认P2 payload，另有协议/加密头；不是端到端延迟硬上限。后台吞吐受64KiB/反馈周期约束，严格优先级在饱和视频负载可饿死P2。
- UDP Ready不证明旁路可达；选择性丢弃全部GDesk而KCP正常的黑洞无自动探测。可靠metadata与UDP到达顺序不保证，HUD可能暂时Unknown或旧backend。
- 未验收Windows/macOS GUI、Flutter完整构建、WAN/NAT、全UDP双向丢包、4K稳态吞吐。GPU接线不等于性能达标，C1报告保留较CPU慢的实测。
- 本次延迟与收敛结论限于记录中的开发构建、256×128画面与负载；不是任意硬件/网络的性能承诺。
