# systemd unit files

`docs/00_design.md` §6.4 + §6.7 不变量 #2 "systemd 是最外层看门狗" 的实施。

## 文件

| Unit | 用途 | bus | docs |
|---|---|---|---|
| `xlerobot-s0-bus1.service` | bus1 8-motor S0 daemon（左臂 6 + 头部 2） | ttyACM0 / 5A7C121427 | `docs/handoffs/2026-04-26_3.md` §5 |
| `xlerobot-s0-bus2.service` | bus2 9-motor S0 daemon（右臂 6 + 底盘 3） | ttyACM1 / 5A7C122340 | `docs/handoffs/2026-04-26_3.md` §5 |

XLeRobot 全栈部署 = 两个 unit 同时跑，daemon 各管一根串口；v0.2 骨架
M=1 单 daemon 多 bus 还没落地（详 `docs/00_design.md` §1.2 + CHANGELOG
2026-04-24 "S0 s0_rt_thread 多 bus 语义澄清"），所以 v0.2 走 "每 bus 一
daemon" 部署。两 daemon 用 unique `robot-id`（`xlerobot_bus1_full` /
`xlerobot_bus2_full`）避 iceoryx2 service collision。

## 通用约定

- 所有 S0 daemon unit 都用 `Type=notify` + `WatchdogSec=2s`，daemon 内
  `crates/robot-s0/src/systemd_notify.rs` 模块按 `WATCHDOG_USEC / 2 = 1s`
  周期 ping，**且**只有 `ipc / rt-or-serial / pub / io` 4 个 worker
  `tick_count` 都还在增长才喂 ping（liveness-gated；详
  [`LivenessObserver`](../../crates/robot-s0/src/systemd_notify.rs)）。
- `Restart=on-failure` —— `kill -9` / panic / watchdog timeout 自动拉起；
  graceful shutdown（`SIGTERM` + clean exit code 0）**不**拉起。
- 日志统一走 journald + JSON（`ROBOT_LOG_FORMAT=json`），方便 vector /
  fluentbit 采集到 Loki / Elastic。
- `User=` / `Group=` 跑 user mode（避 sudo↔user 模式 ownership 残留，详
  `docs/handoffs/2026-04-26.md` §5.2）；`SupplementaryGroups=dialout` 解
  锁串口访问。
- `Conflicts=xlerobot-coordinator.service xlerobot-web.service` 跟 dimos
  旧栈互斥（同一 `ttyACMx` 不能并发占用）。

## 首次部署 — latch provision **必读**

`s0_daemon` 默认走 `MissingLatchPolicy::Strict`：**latch primary + bak
两份都不存在时拒启动**（reviewer round 4 阻断 2 实施 §6.7 #3，详
`crates/robot-s0/src/estop_latch.rs`）。干净机器只创建目录就 `start`
unit 会立刻 `failed` —— 所以首次部署必须**先 provision** 一份 latch
文件（值 = `0`，等价于 "EStop 未触发"）。之后 strict 守门 "rm latch &&
restart" 就**不**等价于"清空 EStop"。

> v0.2 没有 `ClearEStop` control action 入口（v0.3 A1 control-plane slice
> 落地；RPC / Web / 本地维护面板都是同一 action 的 transport 入口，详
> `docs/CHANGELOG.md` 2026-04-26 "v0.3 entry：`ClearEStop RPC` 文案的控制
> 动作解释"）。目前唯一合法的"重设 latch=0"路径就是 deploy 时一次性手动
> provision；这是显式授权流程，不等价于 ClearEStop control action。

### 推荐流程：直接 provision latch=0（无需 opt-in flag）

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

### 替代流程：一次性 `--allow-empty-latch` opt-in

如果 deploy 流程不想手写 latch 文件（CI 部署 / Ansible 等），可以**一次
性**走 drop-in override 加 `--allow-empty-latch` 让 daemon 自动持久化
`latch=0`，**之后立刻移掉 drop-in** 让 strict 接管：

```bash
# (1)~(2) 同上：编辑 unit + 建目录

# 3a) 装 unit
sudo cp scripts/systemd/xlerobot-s0-bus2.service /etc/systemd/system/

# 3b) drop-in 一次性 opt-in
sudo install -d /etc/systemd/system/xlerobot-s0-bus2.service.d
sudo tee /etc/systemd/system/xlerobot-s0-bus2.service.d/10-bootstrap.conf >/dev/null <<'EOF'
[Service]
ExecStart=
ExecStart=/home/loosqk/repo/robot/robot-system/target/release/s0_daemon \
    --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base \
    --config /home/loosqk/repo/robot/robot-system/configs/xlerobot_bus2_full.toml \
    --latch-path /var/lib/robot/xlerobot_bus2_full/estop_latch \
    --tick-hz 50 \
    --allow-empty-latch
EOF
sudo systemctl daemon-reload

# 3c) start：daemon AllowEmpty 看到 latch 缺失 → 默认 false；graceful shutdown
#     时 save_estop_latch 写一份 0\n 进 latch + bak
sudo systemctl start xlerobot-s0-bus2
sleep 3
sudo systemctl stop xlerobot-s0-bus2     # 触发 graceful shutdown 的 latch 持久化
ls -l /var/lib/robot/xlerobot_bus2_full/estop_latch{,.bak}   # 期望两份都在
cat /var/lib/robot/xlerobot_bus2_full/estop_latch    # 期望 "0"

# 3d) **撤掉** drop-in，恢复 strict
sudo rm -rf /etc/systemd/system/xlerobot-s0-bus2.service.d
sudo systemctl daemon-reload

# 3e) strict 模式真启动（latch 已有 = "0"，strict 不阻断）
sudo systemctl start xlerobot-s0-bus2
systemctl is-active xlerobot-s0-bus2   # 期望 active
sudo systemctl enable xlerobot-s0-bus2

# 3f) **strict 守门验证**：手动 rm latch 后再 start 应当 fail
sudo systemctl stop xlerobot-s0-bus2
sudo rm -f /var/lib/robot/xlerobot_bus2_full/estop_latch{,.bak}
sudo systemctl start xlerobot-s0-bus2 || true
systemctl is-active xlerobot-s0-bus2   # 期望 failed (NOT active)
sudo journalctl -u xlerobot-s0-bus2 -n 8 --no-pager -o cat | grep -E "fail-closed|allow-empty"
                                       # 期望看到 ERROR "fail-closed
                                       # (need --allow-empty-latch opt-in or
                                       # ClearEStop control action in v0.3)"
# 验证完后 provision latch=0（按上面"推荐流程" §3 步骤），重启 strict
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
> - 代码：[`LivenessObserver`](../../crates/robot-s0/src/systemd_notify.rs)
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

- `xlerobot-s1.service`（S1 coordinator）、`xlerobot-policy.service` 等
  v0.3+ 的 unit —— 跟着相应 crate 一起在那个 milestone 加。
- 动态生成 unit（unit template `@.service`）—— v0.2 单 robot 部署用静
  态 unit 即可；多机器 / 多 robot 部署再考虑模板化。
- `ClearEStop` control-plane slice（v0.3 A1）—— 落地后 `--allow-empty-latch`
  flag 应当移除，latch=0 重设只能走 `ClearEStop` control action（RPC / Web /
  本地维护面板都是同一 action 的 transport 入口，带 maintainer+ auth +
  confirmation + audit；详 `docs/handoffs/v0.3_entry.md` §3.5）。
