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
  周期 ping。
- `Restart=on-failure` —— `kill -9` / panic / watchdog timeout 自动拉起；
  graceful shutdown（`SIGTERM` + clean exit code 0）**不**拉起。
- 日志统一走 journald + JSON（`ROBOT_LOG_FORMAT=json`），方便 vector /
  fluentbit 采集到 Loki / Elastic。
- `User=` / `Group=` 跑 user mode（避 sudo↔user 模式 ownership 残留，详
  `docs/handoffs/2026-04-26.md` §5.2）；`SupplementaryGroups=dialout` 解
  锁串口访问。
- `Conflicts=xlerobot-coordinator.service xlerobot-web.service` 跟 dimos
  旧栈互斥（同一 `ttyACMx` 不能并发占用）。

## 安装步骤

每个 bus 走相同模板。下面以 bus2 为例；bus1 把 `bus2` 改成 `bus1`，
`xlerobot_bus2_full` 改成 `xlerobot_bus1_full` 即可。

```bash
# 1) 编辑 unit 里的 User= / WorkingDirectory= / ExecStart= 路径，填实机
vim scripts/systemd/xlerobot-s0-bus2.service

# 2) 创建 latch 目录（user 拥有，避免 sudo↔user 残留）
sudo install -d -m 0750 -o $USER -g $USER /var/lib/robot/xlerobot_bus2_full

# 3) 安装到系统
sudo cp scripts/systemd/xlerobot-s0-bus2.service /etc/systemd/system/
sudo systemctl daemon-reload

# 4) 启动 + 开机自启
sudo systemctl start xlerobot-s0-bus2
sudo systemctl status xlerobot-s0-bus2
sudo systemctl enable xlerobot-s0-bus2

# 5) 看日志
journalctl -u xlerobot-s0-bus2 -f
```

### 全栈部署（两 bus 一起跑）

```bash
sudo install -d -m 0750 -o $USER -g $USER /var/lib/robot/xlerobot_bus1_full
sudo install -d -m 0750 -o $USER -g $USER /var/lib/robot/xlerobot_bus2_full
sudo cp scripts/systemd/xlerobot-s0-bus1.service /etc/systemd/system/
sudo cp scripts/systemd/xlerobot-s0-bus2.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start xlerobot-s0-bus1 xlerobot-s0-bus2
sudo systemctl status xlerobot-s0-bus1 xlerobot-s0-bus2
sudo systemctl enable xlerobot-s0-bus1 xlerobot-s0-bus2
```

## 验证 watchdog 重启路径

锁定 `docs/00_design.md` §6.7 不变量 #2 真机成立的硬门槛：

```bash
sudo systemctl start xlerobot-s0-bus2
sleep 5     # 等 daemon 进 Enabled.Active
sudo kill -9 $(pidof s0_daemon)
# 期望：systemctl status xlerobot-s0-bus2 → activating → active（< 5s）
# 期望：journalctl 看到 "READY=1" 重新出现 + state=Ready
sudo systemctl status xlerobot-s0-bus2
```

如果 latch 文件之前 = "1"（前一次 EStop 触发后崩了），重启后 daemon
应当 `initial estop_latched = true` + state 全程 Safe。Step 9 的
[`crash_restart_smoke`](../../crates/robot-tests/src/bin/crash_restart_smoke.rs)
binary 是这条门槛的自动化测试。

## 不在本目录范围内的事

- `xlerobot-s1.service`（S1 coordinator）、`xlerobot-policy.service` 等
  v0.3+ 的 unit —— 跟着相应 crate 一起在那个 milestone 加。
- 动态生成 unit（unit template `@.service`）—— v0.2 单 robot 部署用静
  态 unit 即可；多机器 / 多 robot 部署再考虑模板化。
