# GDesk 生产会话复现

两个独立的 Linux 手动验收场景，均 `include!` 当前工作树的 `src/lib.rs`，保留真实认证、`Connection::start`、`Remote::io_loop`、采集、GPU 编解码与 ACK。观察端实现现有 UI trait，只记录事件、质量状态与解码像素；没有 mock codec、生产 hook 或 Flutter/Sciter GUI。UDP 场景另有隔离的最小 rendezvous 服务，用真实协议完成注册、签名身份和打洞，客户端与服务端仍运行生产会话链路。

所有命令从仓库根执行，Python 3.11+ 使用 `uv run python`。脚本只生成到 `.tmp/gdesk-session-e2e/{credit,udp}/`，包括独立 manifest、lockfile、Rust 入口、target、摘要、运行日志、配置和测试数据。不要提交该目录。不会使用或覆盖两个原始 `.tmp/rfc2-*-e2e/` 证据目录。

## 前置条件

- Linux，`bwrap` 支持非特权 user/net/mount/PID/IPC namespace，`Xvfb`、`xclip`、libX11、可用的 `/dev/dri` Vulkan GPU。
- 已初始化仓库子模块，安装项目正常的 Rust/C++/GPU 构建依赖，Cargo 离线依赖缓存齐全。`--offline` 缺依赖时先按项目方式补齐。
- 没有其他 GPU 会话验收正在运行。四次会话串行执行，避免争抢 GPU 干扰延迟结果。
- 若系统 OpenSSL 不在默认搜索路径，构建前自行设置 `OPENSSL_LIB_DIR`、`OPENSSL_INCLUDE_DIR`。例如 Arch 的兼容 OpenSSL 3 安装可能需要 `/usr/lib/openssl-3` 与 `/usr/include/openssl-3`；脚本不强制这些机器相关路径。默认 `CXXFLAGS=-include cstdint`，允许环境覆盖。

独立 manifest 复用根依赖、patch、profile，禁用根 `build.rs`，避免改写生产版本文件；依赖自身的 protobuf/原生构建正常运行。不能在生产 owner 编辑期间构建。构建输出保存 `build.log`、选定关键源文件的前后 SHA256 及 `binary.sha256`；`source_changed_during_build: true` 的结果应作废后重建。这些摘要不是整个依赖树的快照；完整复现还需保存对应 Git revision 与工作树 diff。

## TCP：20 MiB 剪贴板与 P0/P1 交错

```bash
uv run python tests/gdesk_session_e2e/credit/prepare.py
uv run python tests/gdesk_session_e2e/credit/build.py
uv run python tests/gdesk_session_e2e/credit/run.py
uv run python tests/gdesk_session_e2e/credit/run.py --baseline
```

默认输出分别为 `.tmp/gdesk-session-e2e/credit/runs/clipboard/` 与 `runs/baseline/`。重跑必须指定未存在的新目录；baseline 同时指向对应负载轮次：

```bash
E2E_OUTPUT_DIR="$PWD/.tmp/gdesk-session-e2e/credit/runs/clipboard-2" \
  uv run python tests/gdesk_session_e2e/credit/run.py
E2E_OUTPUT_DIR="$PWD/.tmp/gdesk-session-e2e/credit/runs/baseline-2" \
E2E_BASELINE_REFERENCE="$PWD/.tmp/gdesk-session-e2e/credit/runs/clipboard-2" \
  uv run python tests/gdesk_session_e2e/credit/run.py --baseline
```

私有 TCP 代理限速服务端至客户端 4 MiB/s，计入帧头，以累计字节 deadline 限速。两轮执行相同的 320 次绘图和光标变化，再静止到确定的最终图。负载轮次通过真实 CM `RawMessage` API 注入 20 MiB Clipboard，保留生产调度、分片、credit ACK、重组与客户端业务处理；这不覆盖操作系统侧剪贴板采集。baseline 使用负载轮次的 wire 时间窗口比较视频间隔。

`summary.json` 与 `wire.jsonl` 记录分片、credit ACK、视频、光标、视频 ACK、GPU 后端、最终 RGB MaxError 和视频 gap P99/max。成功退出断言 wire 剪贴板 SHA256 正确、客户端业务接受、每片不超过 16384 字节、负载期间 P0/P1 活动和 GPU 解码最终 MaxError=0。baseline 自动断言负载轮次 P99 不超过基线 P99 加一个64KiB信用窗口在4MiB/s下的串行预算（15.625ms）；这是本受控场景的回归门槛，不是任意网络延迟保证。`clipboard_equal` 是额外 OS xclip 检查；X11 大选择长度限制可能使其失败而 wire/业务断言仍通过，不能将传输成功描述为 OS 20 MiB 粘贴成功。TCP 本场景 `secured=false`。

## UDP：secured KCP、AEAD 视频与丢包收敛

```bash
uv run python tests/gdesk_session_e2e/udp/build.py
uv run python tests/gdesk_session_e2e/udp/run.py
E2E_LOSS=15 uv run python tests/gdesk_session_e2e/udp/run.py
```

`build.py` 将 `observer.rs` 与 `runtime.rs` 原样拼接为临时 `session.rs`。生产 include 相对于临时 manifest 解析，仓库可迁移，无绝对用户路径。无需提交重复的生成入口。

默认输出 `.tmp/gdesk-session-e2e/udp/runs/loss0/` 与 `runs/loss15/`。重跑用 `E2E_OUTPUT_DIR` 指向该场景下未存在的新目录。`E2E_LOSS=0..100` 只对服务端至客户端的 GDesk AEAD 视频包施加确定性伪随机百分比丢包，不主动丢 KCP、打洞或反向 ACK；实际丢包率应以 `dropped/s2c_aead` 计算，不能把名义 15% 当作实测值。

真实 secured KCP 登录并出现 UDP AEAD 解码后，脚本改变私有桌面 8 次，最终静止并在 45 秒窗口内等待逐像素 RGB MaxError=0。断言 secured UDP、GpuVulkan 元数据、AEAD 包不超过 1200 字节、KCP 包解析无错误，以及非零丢包场景确实发生丢包。记录源画面读回、收敛耗时、KCP PUSH/ACK 与重复包计数，并断言收敛后 3 秒 AEAD 尾流量为零、无重复 KCP PUSH、双端未提前退出。失败/超时也必须保留日志，不能改写成验收通过。

## 隔离、清理与验收边界

`bwrap` 挂载宿主根只读，仅本轮输出目录可写；namespace 内 `/tmp`、`/run` 映射到该目录。保留 HOME/CODEX_HOME，使用独立 XDG_CONFIG_HOME、APP_NAME、两套私有 Xvfb 和私有 loopback 网络。GPU 设备单独映射，不开放宿主 uinput。最小 `/etc/passwd` 仅在 namespace 覆盖，避免无 seat 的预登录判断扫描系统账户。使用 click 审批，不含固定密码；UDP 签名密钥和配置只在运行时生成。正常退出及断言失败由 finally 回收脚本创建的子进程，不停止宿主 daemon；若强杀外层进程，bwrap 的 die-with-parent 负责结束隔离环境。

每轮目录拒绝覆盖，日志、运行配置、临时密钥、20 MiB 文本和 target 均留在项目 `.tmp`，不属于提交制品。按需自行删除已结束轮次的目录。

这些是生产业务会话验收入口，不能代替完整 GUI、真实用户桌面、WAN/NAT、TCP 下 UDP 不可用时的自动回退、设备丢失或 OS 大剪贴板验收。历史源脚本与整理后脚本的验证记录应分别保存。

## 不启动 GPU 的整理检查

```bash
uv run python tests/gdesk_session_e2e/credit/prepare.py
uv run python tests/gdesk_session_e2e/udp/build.py --prepare-only
uv run python - <<'PY'
import ast
from pathlib import Path
for script in Path('tests/gdesk_session_e2e').rglob('*.py'):
    ast.parse(script.read_text(), filename=str(script))
print('Python syntax OK')
PY
git diff --check -- tests/gdesk_session_e2e
```

整理阶段已通过全部 Python 脚本语法检查、两套 manifest/lockfile 与依赖路径核对、生成 Rust 入口拼接检查，以及与来源脚本业务逻辑和断言的 AST 一致性检查。生成物均位于项目 `.tmp`，提交目录仅含 8 个源码和文档文件，无日志、配置、密钥、20 MiB 文本或 target。

主代理随后复测整理后脚本：credit build 与含 Clipboard 的 run 均 exit 0，20 MiB hash 正确、RGB MaxError=0；baseline 正在执行。UDP build 已 exit 0，15% 丢包会话等待 baseline 完成后串行执行。具体指标、最终结论与日志位置由主代理验收完成后补充；尚未完成的轮次不计为通过。
