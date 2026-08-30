# systemd unit files

`docs/00_design.md` §6.4 + §6.7 不变量 #2 "systemd 是最外层看门狗" 的实施。

> **v0.5 候选 E 的 arm-only 双栈部署（unit ×7 + handoff transaction +
> guard 启动链）在 [`README_dimos_replace.md`](README_dimos_replace.md)。**
> 本文件继续覆盖旧 full-bus unit（`xlerobot-s0-bus{1,2}.service`）与各
> smoke 模板——旧 unit **保留不删不改**作回滚路径。

## 文件

| Unit | 用途 | bus | docs |
|---|---|---|---|
| `xlerobot-s0-bus1.service` | bus1 8-motor S0 daemon（左臂 6 + 头部 2） | ttyACM0 / 5A7C121427 | `docs/handoffs/2026-04-26_3.md` §5 |
| `xlerobot-s0-bus2.service` | bus2 9-motor S0 daemon（右臂 6 + 底盘 3） | ttyACM1 / 5A7C122340 | `docs/handoffs/2026-04-26_3.md` §5 |

### 测试 unit 模板（**不要直接 start**；由 smoke 渲染占位符后装进 `$XDG_RUNTIME_DIR/systemd/user/`）

| 模板 | daemon | smoke driver | 落地 |
|---|---|---|---|
| `xlerobot-s0-freeze-test.service` | `s0_daemon`（`--features mock-only,test-freeze-hook`） | `crates/robot-tests/src/bin/freeze_systemd_smoke.rs` | v0.3 A2-3 |
| `xlerobot-s1-freeze-test.service` | `s1_daemon`（`--features test-freeze-hook`） | `crates/robot-tests/src/bin/sd_watchdog_smoke.rs`（`--phase s1`） | v0.5 候选 E 段(8) |
| `xlerobot-gateway-freeze-test.service` | `robot_rpc_gateway`（`--features test-freeze-hook`） | 同上（`--phase gateway`） | v0.5 候选 E 段(8) |

XLeRobot 全栈部署 = 两个 unit 同时跑，daemon 各管一根串口；v0.2 骨架
M=1 单 daemon 多 bus 还没落地（详 `docs/00_design.md` §1.2 + CHANGELOG
2026-04-24 "S0 s0_rt_thread 多 bus 语义澄清"），所以 v0.2 走 "每 bus 一
daemon" 部署。两 daemon 用 unique `robot-id`（`xlerobot_bus1_full` /
`xlerobot_bus2_full`）避 iceoryx2 service collision。

## 通用约定

- 所有 S0 daemon unit 都用 `Type=notify` + `WatchdogSec=2s`，daemon 内
  `crates/robot-health/src/systemd.rs` 模块（段(8) 起单一实现，原 `crates/robot-s0/src/systemd_notify.rs`）按 `WATCHDOG_USEC / 2 = 1s`
  周期 ping，**且**只有 `ipc / rt-or-serial / pub / io` 4 个 worker
  `tick_count` 都还在增长才喂 ping（liveness-gated；详
  [`LivenessObserver`](../../crates/robot-health/src/systemd.rs)）。
- `Restart=on-failure` —— `kill -9` / panic / watchdog timeout 自动拉起；
  graceful shutdown（`SIGTERM` + clean exit code 0）**不**拉起。
- **v0.5 候选 E 段(8) 起 S1 / gateway 同款**（Spec D12 / P.2.7）：`s1_daemon`
  与 `robot_rpc_gateway` 走同一 `robot-health::systemd` 实现——全部 worker
  就绪后 `READY=1`（gateway 额外要求 tonic loopback 探针首成，
  `crates/robot-rpc/src/systemd_probe.rs`）、liveness-gated `WATCHDOG=1`
  （S1：tick loop / heartbeat / 4 rr / state cache；gateway：6 rr_io /
  estop_pub / **每 hardware 一条** state_pool / 探针 success_count）、退出
  先 `STOPPING=1`。生产 S1 / gateway unit 属段(9) 部署件；本目录当前只
  有二者的**测试模板**（上表）。
- 日志统一走 journald + JSON（`ROBOT_LOG_FORMAT=json`），方便 vector /
  fluentbit 采集到 Loki / Elastic。
- `User=` / `Group=` 跑 user mode（避 sudo↔user 模式 ownership 残留，详
  `docs/handoffs/2026-04-26.md` §5.2）；`SupplementaryGroups=dialout` 解
  锁串口访问。
- `Conflicts=xlerobot-coordinator.service xlerobot-web.service` 跟 dimos
  旧栈互斥（同一 `ttyACMx` 不能并发占用）。

## 首次部署 — latch provision **必读**

`s0_daemon` 走 strict fail-closed：**latch primary + bak 两份都不存在
时拒启动**（reviewer round 4 阻断 2 实施 §6.7 #3，A1-5 撤掉
`--allow-empty-latch` opt-in 后锁死为唯一行为；详
`crates/robot-s0/src/estop_latch.rs`）。干净机器只创建目录就 `start`
unit 会立刻 `failed` —— 所以首次部署必须**先 provision** 一份 latch
文件（值 = `0`，等价于 "EStop 未触发"）。之后 strict 守门 "rm latch &&
restart" 就**不**等价于"清空 EStop"。

> 真清 latch 路径 = (a) 首次部署 deploy 流程显式 provision `0\n`（这是
> 显式授权流程，不等价于 ClearEStop control action），或 (b) daemon
> 运行期通过 v0.3 A1 `ClearEStop` control action 真清（gRPC / Web /
> 本地维护面板都是同一 action 的 transport 入口；详
> `docs/CHANGELOG.md` 2026-04-26 "v0.3 entry：`ClearEStop RPC` 文案的
> 控制动作解释"）。

### deploy 流程：provision latch=0

```bash
# 1) 编辑 unit 里的 User= / WorkingDirectory= / ExecStart= 路径，填实机
vim scripts/systemd/xlerobot-s0-bus2.service

# 2) 创建 latch 目录（user 拥有，避免 sudo↔user 残留）
sudo install -d -m 0750 -o $USER -g $USER /var/lib/robot/xlerobot_bus2_full

# 3) **provision** primary + backup 双文件 = "0\n"（latch 未触发）
LATCH=/var/lib/robot/xlerobot_bus2_full/estop_latch
printf '0\n' | sudo tee "$LATCH" "$LATCH.bak" >/dev/null
sudo chown $USER:$USER "$LATCH" "$LATCH.bak"
sudo chmod 0640 "$LATCH" "$LATCH.bak"

# 4) 安装到系统
sudo cp scripts/systemd/xlerobot-s0-bus2.service /etc/systemd/system/
sudo systemctl daemon-reload

# 5) 启动 + 开机自启
sudo systemctl start xlerobot-s0-bus2
systemctl is-active xlerobot-s0-bus2   # 期望 active
sudo systemctl enable xlerobot-s0-bus2

# 6) 看日志
journalctl -u xlerobot-s0-bus2 -f
```

### strict 守门验证（可选 sanity）

```bash
# 手动 rm latch 后再 start 应当 fail
sudo systemctl stop xlerobot-s0-bus2
sudo rm -f /var/lib/robot/xlerobot_bus2_full/estop_latch{,.bak}
sudo systemctl start xlerobot-s0-bus2 || true
systemctl is-active xlerobot-s0-bus2   # 期望 failed (NOT active)
sudo journalctl -u xlerobot-s0-bus2 -n 8 --no-pager -o cat | grep -E "fail-closed|both missing"
                                       # 期望看到 ERROR "fail-closed
                                       # (provision `0\n` to both files for first
                                       # deploy, or use ClearEStop control action
                                       # at runtime)"
# 验证完后再次 provision latch=0（按上面"deploy 流程" §3 步骤）。
```

### 全栈部署（两 bus 一起跑）

跑两遍上面"推荐流程"，bus1 把 `bus2` 替换成 `bus1`，
`xlerobot_bus2_full` 替换成 `xlerobot_bus1_full`，`right_arm_full_with_base`
替换成 `left_arm_full_with_head`。然后：

```bash
sudo systemctl start xlerobot-s0-bus1 xlerobot-s0-bus2
systemctl is-active xlerobot-s0-bus1 xlerobot-s0-bus2   # 都期望 active
sudo systemctl enable xlerobot-s0-bus1 xlerobot-s0-bus2
```

## 验证 systemd 重启路径（**全套已锁** — `Restart=on-failure` + watchdog cap-deadlock）

> **宣称范围**（reviewer A2-3 / A2-4 复核后边界）：本节验证 §6.7 #2
> "systemd 是最外层看门狗" 的**两条** systemd 重启路径都已端到端真锁住：
>
> 1. **进程死亡 → `Restart=on-failure` 拉起**（v0.2 收尾已锁，详 §6.1
>    手动 kill -9 流程 + `crash_restart_smoke` 5/5 PASS）。
> 2. **worker cap-deadlock → liveness-gated watchdog 不喂 ping →
>    `WatchdogSec` timeout → `Restart=on-failure` 拉起新 daemon**
>    （v0.3 A2 落地）。
>
> v0.3 A2 闭合的具体证据：
>
> - 代码：[`LivenessObserver`](../../crates/robot-health/src/systemd.rs)
>   观测 `ipc / rt-or-serial / pub / io` 4 worker `tick_count`；任一停
>   滞就跳 `sd_notify(WATCHDOG=1)`。
> - 端到端 smoke：`crates/robot-tests/src/bin/freeze_systemd_smoke.rs`
>   起 user-mode systemd unit (`Type=notify` + `WatchdogSec=2s` +
>   `Restart=on-failure`) → 通过 test-only iceoryx2 topic
>   `/<robot_id>/test/freeze` publish freeze 命令 → ipc_thread 真 spin
>   5s → daemon liveness-gated watchdog 真停 ping → systemd
>   `WatchdogSec=2s` 真超时 → SIGABRT → `Restart=on-failure` 真拉新
>   daemon。
> - 真机回归：FRIDAY workstation 本机 5/5 PASS + **Orin AGX 1 round
>   PASS**（详 `docs/handoffs/2026-04-26_6.md` §5）。
> - 真宣称升级走 [`docs/CHANGELOG.md`](../../docs/CHANGELOG.md) v0.3 偏
>   离登记（v0.2 "代码层面到位" → v0.3 A2 "真锁住"），**不**改
>   `docs/00_design.md` 正文。

### 跑 cap-deadlock 真锁路径自动化测试（A2-3 smoke）

```bash
# 1) 编 daemon binary（必须显式带 test-freeze-hook feature；不带 default
#    避免后续 cargo build --workspace 用默认 feature 覆盖产物）
cargo build -p robot-s0 --bin s0_daemon \
    --no-default-features --features mock-only,test-freeze-hook

# 2) 跑 smoke（默认 5 round；--user 域，不需要 sudo）
cargo run -p robot-tests --bin freeze_systemd_smoke \
    --features freeze-systemd-smoke -- \
    --daemon-bin target/debug/s0_daemon \
    --iterations 5
# 期望：每 iter PID 切换 + is-active 二次确认 + 4 条 journal 硬断言：
#   withholding=1 starting=2 freeze_ready=2 state_ready=2
```

### 跑 S1 / gateway watchdog **双向** smoke（v0.5 候选 E 段(8)，AC-E19）

```bash
# 1) 编两个 daemon（必须显式带 test-freeze-hook；release 与 gate 同款产物）
cargo build --release -p robot-s1  --bin s1_daemon         --features test-freeze-hook
cargo build --release -p robot-rpc --bin robot_rpc_gateway --features test-freeze-hook

# 2) 跑 smoke（--user 域，不需要 sudo；默认 8 case = S1 ×4 + gateway ×4）
cargo run -p robot-tests --bin sd_watchdog_smoke --features sd-watchdog-smoke -- \
    --s1-bin target/release/s1_daemon \
    --gateway-bin target/release/robot_rpc_gateway \
    --phase all
# 期望：每 case ✓ READY → 注入 freeze（target_pid = PID0）→ withholding →
#   PID 切换 + is-active → journal（withholding ≥1 / banner ≥2 / "READY=1
#   sent" ≥2 / "freeze_control thread ready" ≥2 / applied ≥1）→ 反向步
#   （PID0 定向命令 → PID1 "stale freeze ignored (pid mismatch)"，applied
#   不变）→ 空闲 3 × WatchdogSec 不重启、无二次 withholding；total 8 PASS。

# 3) 跑完后恢复不带 hook 的 release 产物（避免混版本，RS2 教训）
cargo build --release --workspace
```

默认 worker 集：S1 `tick,heartbeat_pub,rr_server:0,state_cache`；gateway
`rr_io:0,estop_pub,state_pool:1,tonic_probe`（`state_pool:1` = 双 hardware
config 冻**第二条** state_pool 线程，证明健康线程不掩蔽卡死线程，R6-5）。
`--s1-workers` / `--gateway-workers` 可改集合（`rr_server:<0-3>` /
`rr_io:<0-5>` / `state_pool:<0-1>`）。

**注意**：本 unit **只**启 S0，**不**启 S1 coordinator。S0 daemon 没有
`Heartbeat` 输入源时最多走到 `state=Ready`，**不**会自己进
`Enabled.Active`（Active 必须 S1 发心跳）。

```bash
sudo systemctl start xlerobot-s0-bus2
sleep 3     # 等 daemon 启动期 (factory build + 4 thread spawn + sd_notify READY=1)
systemctl is-active xlerobot-s0-bus2   # 期望 active
sudo journalctl -u xlerobot-s0-bus2 -n 5 --no-pager -o cat | grep -E "state.*Ready|heartbeat"
            # 期望：tracing JSON 行 "message":"heartbeat","state":"Ready",...

# **进程死亡 → systemd Restart=on-failure 路径**（本节锁定的就这条）
sudo kill -9 $(systemctl show -p MainPID --value xlerobot-s0-bus2)
sleep 4     # 等 systemd 检测 SIGCHLD + Restart=on-failure 拉起新 daemon
            # 4s = 2s WatchdogSec + 2s RestartSec 余量

systemctl is-active xlerobot-s0-bus2   # 期望 active（新 PID）
sudo journalctl -u xlerobot-s0-bus2 -n 10 --no-pager -o cat | grep -E "s0_daemon starting|state.*Ready"
            # 期望：看到第二次 "s0_daemon starting" + 第二次 state=Ready
sudo systemctl status xlerobot-s0-bus2 --no-pager
```

`crash_restart_smoke` 是这条路径 + latch 持久化的自动化端到端测试（详
[`crates/robot-tests/src/bin/crash_restart_smoke.rs`](../../crates/robot-tests/src/bin/crash_restart_smoke.rs)）；
按结构化 tracing 字段断言"重启 daemon 读到 `initial estop_latched`
+ `latched=true`"，兼容 human + JSON 两种输出。

**v0.3 A2 已真锁住 §6.7 #2 cap-deadlock 路径**：见本节顶部"全套已锁"
段 + 上面 "跑 cap-deadlock 真锁路径自动化测试" 子节。`crash_restart_smoke`
覆盖路径 1（进程死亡 → `Restart=on-failure`）；`freeze_systemd_smoke`
覆盖路径 2（worker cap-deadlock → liveness-gated watchdog → `WatchdogSec`
timeout → `Restart=on-failure`）。

## 不在本目录范围内的事

- 生产 `xlerobot-s1-*.service`（S1 coordinator）/ gateway unit ——
  v0.5 候选 E 段(9) 部署件（daemon 侧 sd_notify 已在段(8) 落地，见上）；
  `xlerobot-policy.service` 等更后 milestone 的 unit 跟着相应 crate 加。
- 动态生成 unit（unit template `@.service`）—— v0.2 单 robot 部署用静
  态 unit 即可；多机器 / 多 robot 部署再考虑模板化。
- `--allow-empty-latch` opt-in flag — A1-5 已撤；唯一守门 = strict
  fail-closed + 上文 "首次部署" 段的运维 provision 流程；运行期清 latch
  只能走 v0.3 A1 `ClearEStop` control action（RPC / Web / 本地维护面板
  都是同一 action 的 transport 入口，带 maintainer+ auth + confirmation
  + audit）。
