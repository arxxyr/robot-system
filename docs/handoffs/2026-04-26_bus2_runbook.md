# Handoff · 2026-04-26 · bus2 right_arm 6-motor 真机闭环 Runbook

> **状态：✅ task #15 Stage 1 真机验证全过（2026-04-26）**。
> bus2 右臂 6 motor SerialRT 路径完整闭环，§2-§7 全部通过。Stage 2（加 base
> 3 wheel = 9 motor 全栈）待 mock_s1 加 "部分关节冻结" 模式后再做。
>
> 实测结果详见 [`2026-04-26.md`](2026-04-26.md) handoff §3。本 runbook 在
> 实测后已根据踩坑经验更新（命令 typo 修复 + sudo↔user 模式选择 + 多个
> known issue / workaround）。下次跑可直接按本 runbook 执行。

---

## 0. 前提

- Orin（10.0.1.41）已部署 dimos-deploy；本 runbook 跑前**必须**先停 dimos
  service 释放硬件占用：

  ```bash
  ssh loosqk@10.0.1.41
  sudo systemctl stop xlerobot-coordinator xlerobot-web
  systemctl is-active xlerobot-coordinator   # 确认 inactive
  ```

- robot-system 仓库已 `git pull` 到最新（含 task-30 v9 M7 + SerialRT E2E
  修复 + task #15 latch fsync 修复，commit `60ba241` 及之后）。

- bus2 接 5A7C122340（右臂 + 底盘）。`/dev/serial/by-id/usb-1a86_USB_
  Single_Serial_5A7C122340-if00` 必须存在；用 `ls -l /dev/serial/by-id/`
  确认。

### 0.1 模式选择（2026-04-26 实测踩坑教训）

**强烈建议全用 user 模式**（daemon / mock_s1 / robotctl 都不 sudo）。
原因：

1. dimos service 关闭后 `/dev/ttyACM1` 是 `dialout` group，`loosqk`
   用户已在 `dialout` 组（`groups | grep dialout`），不需 sudo 就能开。
2. `s0_daemon` 默认 `strict_rt=false`，不需要 SCHED_FIFO / mlockall，
   也就不需要 `CAP_SYS_NICE` / `CAP_IPC_LOCK`。
3. **sudo daemon + user clients 混跑会暴露两个真 bug**（实测 2026-04-26
   多次复现）：
   - `sudo daemon` 写的 latch 文件 owner=root → 后续 `user daemon` 读
     `Permission denied (os error 13)` → daemon 直接 fatal 退出。
   - `sudo daemon` 创建的 `/tmp/iceoryx2/nodes/<uuid>/` owner=root →
     `user mock_s1` 或 `user robotctl` open service 报
     `PublishSubscribeOpenError(ServiceInCorruptedState)`。

**如果非要混 sudo / user 跑**（不建议，但有些场景需要 SCHED_FIFO），切换
之间必须做：
```bash
sudo rm -f <latch_path> <latch_path>.bak
sudo rm -rf /tmp/iceoryx2/nodes/* /tmp/iceoryx2/services/*
```

### 0.2 latch 路径选择

User 模式下 latch 路径用项目内 `logs/` 最方便（user 可读写）：

```bash
mkdir -p ~/repo/robot/robot-system/logs
# 跑命令时传 --latch-path logs/estop_latch_bus2_right_arm
```

如果坚持用 `/var/lib/robot/<id>/`（系统部署 / systemd 服务化场景），
**user 模式 daemon 必须**先用 sudo 把目录建出来 + 改 owner：
```bash
sudo install -d -m 0750 -o loosqk -g loosqk /var/lib/robot/xlerobot_bus2_right_arm
```
（注意 `-o loosqk -g loosqk`，让 user 可写。`s0_daemon` 启动期也调
`ensure_latch_parent` 守门做 `create_dir_all`，但**不**修 owner —— sudo
建目录时记得指定 owner。）

---

## 1. 真机编译（Orin 本地）

Orin = aarch64，本机交叉编译麻烦；直接 Orin 本地 build。

```bash
cd ~/repo/robot/robot-system
git fetch && git checkout master && git pull --ff-only
source ~/.cargo/env    # ssh 非交互 shell 可能没自动加载 cargo PATH

# 如果没装 sccache，先装一次（task-30 v9 落地后已加 .cargo/config.toml
# 的 rustc-wrapper=sccache；缺 sccache 会 build fail）：
which sccache || cargo install sccache

# release + xlerobot-real（拉 serialport / libudev）
cargo build --release -p robot-s0 --no-default-features --features xlerobot-real \
    -p robot-s1 -p robot-cli
cargo build --release --example feetech_poke -p robot-adapter-feetech --features real-hw
```

期望：2 个 build 都过；`target/release/{s0_daemon,mock_s1,robotctl,examples/feetech_poke}` 全在。

**如果一定不想装 sccache**，单次 build 加 `CARGO_BUILD_RUSTC_WRAPPER=` 显式
覆盖（实测 2026-04-26 user 在 sccache 没装时用此招，2 分钟 build 完）：
```bash
CARGO_BUILD_RUSTC_WRAPPER= cargo build --release ...
```

---

## 2. bus2 全 motor handshake（feetech_poke ping）

先确认 bus2 上**所有** motor 都通信正常。`feetech_poke ping` 逐 motor
ping，能看到回包。

**注意：`feetech_poke` 是 positional CLI**（**不是** `--address ... --motor-id ...`，
runbook 旧文案有误，本节已校正）：

```text
target/release/examples/feetech_poke <address> <motor_id> ping
target/release/examples/feetech_poke <address> <motor_id> read  <register>
target/release/examples/feetech_poke <address> <motor_id> write <register> <value>
```

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00

# right_arm 6 motor (id 1..=6)
for id in 1 2 3 4 5 6; do
    target/release/examples/feetech_poke "$BUS" "$id" ping
done

# base 3 wheel (id 7, 8, 9) —— 不做后续驱动，但 ping 通是 bus 健康基线
for id in 7 8 9; do
    target/release/examples/feetech_poke "$BUS" "$id" ping
done
```

**通过条件**（2026-04-26 实测，9 个 motor 全 PASS）：所有 motor 返回
`ping ok: motor_id=<id> on <bus> (Model_Number=STS3215 777)`。

**失败排查**：
- ping fail → 物理连线 / 电源 / motor ID 配置问题（不是 software bug，让
  dimos 旧栈也 ping，对比。dimos `feetech_io` 同款行为）。
- 部分通部分不通 → 看哪些 ID 出问题。

**附**：可同款 `read Torque_Enable` 拿 baseline，方便后续 §5 EStop 对比：
```bash
for id in 1 2 3 4 5 6; do
    target/release/examples/feetech_poke "$BUS" "$id" read Torque_Enable
done
```

---

## 3. Smoke：s0_daemon 不发命令 5s

第一关：**只**起 s0_daemon，**不**起 mock_s1，**不**应该有任何 motor 动作。
state 应当停在 `Ready`（没收到 heartbeat 走不进 Enabled）。

```bash
target/release/s0_daemon \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --config configs/xlerobot_bus2_right_arm_full.toml \
    --latch-path logs/estop_latch_bus2_right_arm \
    --smoke-duration 5s 2>&1 | tee logs/bus2_smoke.log
```

期望日志（关键行）：
```
feetech factory (SerialRT): built 'right_arm_full' adapter on
  /dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00
  (motors 6 | ids [1, 2, 3, 4, 5, 6] | calibration=loaded)
heartbeat: state=Ready  guards.estop=false  guards.hb_ok=false  t=2.00...
heartbeat: state=Ready  guards.estop=false  guards.hb_ok=false  t=4.00...
smoke duration elapsed → Event::Stop
```

**验证项**（**必须全过**）：
1. ✅ factory 报 `(SerialRT)`（不是 HardRT 路径）—— 证明 task-30 v9 M7 阶段
   4 第二步 dispatch 工作正常。
2. ✅ `motors 6 | ids [1, 2, 3, 4, 5, 6]`。
3. ✅ `calibration=loaded`（不是 `none`）。
4. ✅ state=Ready（不是 Safe / Fault；如 Safe 则 latch 文件还在，看 §6 重置）。
5. ✅ **5s 内右臂物理上**完全不动**（adapter `enable(true)` 没被调，电机
   torque off，自由下垂或机架支撑）。
6. ✅ `clean shutdown`，无 panic / fatal。

---

## 4. mock_s1 6-joint ballet 12s（核心闭环）

跑 6 关节小幅正弦（±5° = ±0.087 rad），让右臂物理"摆 ballet"。

**两个终端**：

终端 A（s0_daemon）：
```bash
target/release/s0_daemon \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --config configs/xlerobot_bus2_right_arm_full.toml \
    --latch-path logs/estop_latch_bus2_right_arm \
    --tick-hz 50 \
    --smoke-duration 14s 2>&1 | tee logs/bus2_ballet_s0.log
```

终端 B（mock_s1，等 s0_daemon 启动后 ~1s 再启）：
```bash
sleep 1
target/release/mock_s1 \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --joints 6 \
    --amplitude 0.087 \
    --frequency 0.25 \
    --anchor-timeout-ms 1000 \
    --duration 12s 2>&1 | tee logs/bus2_ballet_s1.log
```

终端 C（robotctl watch，可选）：
```bash
target/release/robotctl watch \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --interval-ms 250 \
    --duration-secs 13 2>&1 | tee logs/bus2_ballet_status.log
```

期望物理观察：
- **0–1s**：daemon 启动，bus2 6 motor connect，**仍不动**。
- **1–2s**（mock_s1 启动 + anchor）：mock_s1 读 S0 state 第一帧，bias=current。
  daemon `auto Event::Enable` + 收到 heartbeat → READY → ENABLED.Active →
  serial_io_thread 接 Enable(true) 真上扭。**右臂瞬间锁定当前位姿**，target
  ≈ current，**不跳**。
- **2–14s**：6 关节同步正弦摆动 ±5°，4 秒一周期（0.25 Hz）。肉眼像"右臂
  ballet"。
- **14s**：smoke 超时，daemon 走 STOPPING → 停 mock_s1 / robotctl。

期望日志关键点：
```
# s0_daemon 日志
feetech factory (SerialRT): built 'right_arm_full' ...
spawn_serial_io_thread: rt_class=SerialRT ...
state=Enabled.Active ...
state_changes 计数 >= 2

# mock_s1 日志
anchor: bias = [+0.123, -0.045, +0.012, ...]    ← 6 个 bias，对应 current pos
publishing: target = [..., ..., ...]            ← 50 Hz × 12s = ~600 帧

# robotctl status
state: Enabled.Active
control_link: Ok
output_mode: Active
safety_state: TorqueOn       ← SerialRT mock 同款；Feetech 真 motor 也 = TorqueOn
fault_code: Unknown
```

**验证项**：
1. ✅ 启动期 0–1s 不动；anchor 之后**不跳**。
2. ✅ 12s 内 6 关节都摆动，幅度肉眼 ≈ ±5°。
3. ✅ wire `S0SafetyState=TorqueOn`（rt_thread / serial_io_thread 推上来；任何
   confirm/StopFailed 都说明有问题）。
4. ✅ wire `state=Enabled.Active`，`output_mode=Active`，`fault_code=Unknown`。
5. ✅ shutdown 干净，retire 后所有 motor torque off（`feetech_poke read`
   Torque_Enable=0）。

---

## 5. EStop 真机端到端（robotctl estop）

跑动期间触发 EStop topic，看右臂**立即**断扭（< 1 秒，肉眼"啪一下"软掉）。

`robotctl estop` 已在本批次落地：发一帧 `EStopEvent` 到
`/<robot_id>/events/estop`，daemon `ipc_thread::poll_estop` 拿到 →
`ipc_tick` emergency drain → `executor_ctrl.safety_stop()` →
`serial_io_thread` swap `emergency_action` → adapter `safety_stop_fast`
（Feetech = broadcast `Torque_Enable=0`，< 10 ms）。

**4 个终端**：

终端 A（s0_daemon）：
```bash
target/release/s0_daemon \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --config configs/xlerobot_bus2_right_arm_full.toml \
    --latch-path logs/estop_latch_bus2_right_arm \
    --tick-hz 50 \
    --smoke-duration 14s 2>&1 | tee logs/bus2_estop_s0.log
```

终端 B（mock_s1，sleep 1s 启动）：
```bash
sleep 1
target/release/mock_s1 \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --joints 6 \
    --amplitude 0.087 \
    --frequency 0.25 \
    --anchor-timeout-ms 1000 \
    --duration 12s 2>&1 | tee logs/bus2_estop_s1.log
```

终端 C（robotctl watch）：
```bash
target/release/robotctl watch \
    --robot-id xlerobot_bus2_right_arm \
    --hardware-id right_arm_full \
    --interval-ms 200 \
    --duration-secs 13 2>&1 | tee logs/bus2_estop_status.log
```

终端 D（**等右臂摆起来 ~5s 后**触发 EStop）：
```bash
sleep 6
target/release/robotctl estop \
    --robot-id xlerobot_bus2_right_arm \
    --reason "task15-bus2-runbook" 2>&1 | tee logs/bus2_estop_trigger.log
```

期望物理观察：
- t=0..6s：右臂正常 ballet。
- t=6s：robotctl estop 发出。**< 100 ms 内右臂"啪"一下软掉**（broadcast
  Torque_Enable=0 已到 6 motor）。手指捏关节能感觉到无阻力（torque off）。
- t=6s..14s：右臂保持软状态；mock_s1 仍在发 command 但 daemon 已进 Safe
  → policy=StopTorque → serial_io_thread 不下 write_command。

期望日志关键点：
```
# robotctl estop 终端
robotctl estop: published 1 EStopEvent (stamp_ns=...); daemon should
   emergency-drain within next ipc tick + serial/rt tick (≈ 10–50 ms).

# robotctl watch 终端（estop 之后的快照）
state: Safe                      ← 从 Enabled.Active 跳到 Safe
guards.estop=true                ← latch 已落 guard
output_mode: Unknown
safety_state: StopCommanded → TorqueOffConfirmed
                                 ← serial 端 confirm_torque_off 链路推进
fault_code: Unknown              ← 没升 Fault；EStop 是正常事件路径

# s0_daemon 日志
state_changes 计数 (estop 之后): >= 3   ← Active→Safe (+ 可能进 Stopping)
```

**验证项**：
1. ✅ EStop 之后右臂**< 1s 内**物理软掉（手指验证）。
2. ✅ wire `state=Safe`, `guards.estop_latched=true`。
3. ✅ wire `S0SafetyState` 在几个 tick 内升 `TorqueOffConfirmed`（serial
   端 confirm 真断扭）。
4. ✅ wire `S0FaultCode=Unknown`（EStop **不**走 Fault 路径，这是正常
   emergency 处理）。
5. ✅ **触发即落盘**（**daemon 仍在运行**就读 latch 文件，**不**等
   shutdown）：
   ```bash
   # robotctl estop 发出后立即（< 100 ms）执行：
   sudo cat logs/estop_latch_bus2_right_arm
   # 期望输出：1
   sudo cat logs/estop_latch_bus2_right_arm.bak
   # 期望输出：1
   ```
   这条门槛是 task #15 阻断 1 修复后的硬要求：旧实现仅在 graceful
   shutdown 写 latch，daemon 在 graceful 退出前崩溃会丢 latch；本批
   `ipc_thread` 加了"`guards.estop_latched` 翻转即 `save_estop_latch`"
   hook + 锁测 `estop_topic_writes_latch_to_disk_immediately` 钉死。

---

## 6. EStop latch 重启恢复（待 §5 通过后做）

> 跑前提：`logs/estop_latch_bus2_right_arm` **不**存在
> （或内容为 `0\n`）。

1. 触发 EStop（§5）让 daemon 写 latch=true。
2. `cat logs/estop_latch_bus2_right_arm` →`1`。
3. 重启 daemon（`Ctrl+C` 之前那次，再 launch 一次）→ 启动期日志 `EStop
   latch loaded: latched=true`，state=Safe。
4. mock_s1 发 heartbeat 12s → state 应**留在 Safe**（latch 没清，
   ready_guards_pass 不通），右臂**全程不动**。

## 7. 手动 ClearEStop（latch 文件清）

v0.2 没接 ClearEStop RPC；手动等价 = 清 latch 文件 + 重启。

```bash
sudo rm logs/estop_latch_bus2_right_arm \
        logs/estop_latch_bus2_right_arm.bak
# 重新跑 §4 (mock_s1 ballet)
```

期望：daemon 启动期日志 `EStop latch: not found, latched=false`，
state=Safe→Ready→Enabled.Active 链路恢复，正弦继续。

---

## 8. 报回回执模板

2026-04-26 实测回执（已 PASS，留下次跑参考）：

```
=== bus2 6-motor §3 Smoke ===  PASS
factory: feetech factory (SerialRT): built 'right_arm_full' adapter on
  ...122340-if00 (motors 6 | ids [1, 2, 3, 4, 5, 6] | calibration=loaded)
heartbeat: state=Ready @t=2.0s, 4.0s
shutdown: smoke duration elapsed → Event::Stop / s0_daemon: clean shutdown
物理: 右臂全程不动；6 motor Torque_Enable 维持 baseline (1)

=== bus2 6-motor §4 Ballet ===  PASS
daemon state: Ready (t=0..4s) → Enabled.Active (t=6..14s, 8s 全程持续)
mock_s1 anchor: bias=[-0.014, -1.677, 1.453, 0.319, -1.713, -1.008]
mock_s1 cmd: cmd_seq=788 (16s × 50Hz)
post-run motor pos delta: motor 2 +37 raw (+3.3°), motor 4 +46 raw (+4.0°)
                          其他 < 1°（hold drift 范围）
==> PASS（amplitude ±5° 范围内的真摆动）

=== bus2 6-motor §5 EStop ===  PASS（task #15 阻断 1 真硬件成立）
robotctl estop: published 1 EStopEvent
T+200ms (daemon 仍 active YES): primary=1, bak=1   ← fsync 双写已落盘
daemon: state=Safe guards.estop=true @t=12s
post-shutdown motor Torque_Enable 全 6 = 0   ← broadcast Torque_Enable=0 真断扭

=== bus2 6-motor §6 latch=true 重启 ===  PASS（5/5）
initial estop_latched = true
state 全程 Safe (t=2..12s)，即便 hb_ok=true 也不进 Enabled
6 motor Torque_Enable=0 全程不变；无任何 Enabled 转换

=== bus2 6-motor §7 删 latch 恢复 + 再 EStop ===  PASS（5/5 step）
1) motor 全 0 ✓
2) rm primary + bak ✓
3) daemon 单跑 → estop_latched=false + state=Ready ✓
4) +mock_s1 → t=6s state=Enabled.Active ✓
5) robotctl estop → post-shutdown 6 motor Torque_Enable=0 ✓
```

---

未来跑时如有 fail，按下面格式回执：

```
=== bus2 6-motor §3 Smoke ===
factory line: <粘贴 'feetech factory (SerialRT) ...' 那行>
heartbeat snapshots: <粘贴 2 行 'heartbeat: state=...' >
shutdown line: <粘贴 'smoke duration elapsed' / 'clean shutdown'>
物理观察: 右臂全程 [ 完全不动 / 有抖动（具体描述） ] 
==>  PASS / FAIL（如 FAIL 写原因）

=== bus2 6-motor §4 Ballet ===
state_changes 计数: <从 metrics 取，或粘贴 daemon 输出最末行>
mock_s1 anchor 行: <'anchor: bias = [...]'>
robotctl watch 节选: <粘贴 1-2 行最稳态 'state=Enabled.Active ...'>
物理观察: 6 关节摆动 [ 同步正弦 ±5° / 不同步 / 部分关节抖动 / 跳起 / ... ]
shutdown 后 torque: feetech_poke read Torque_Enable=<0/1>
==>  PASS / FAIL

=== bus2 6-motor §5 EStop ===
estop 触发时刻: t=<秒>
right_arm 软掉延迟: <估算 ms>
estop 后 watch 节选: <粘贴 'state=Safe ... safety_state=TorqueOffConfirmed'>
shutdown 后 latch 文件: <粘贴 cat /var/lib/.../estop_latch 输出>
==>  PASS / FAIL

=== bus2 6-motor §6 latch 恢复 ===
重启后第一 status: <粘贴 'state=Safe guards.estop=true'>
mock_s1 12s 期间右臂动作: [ 完全不动 / 抖动 / 跑了 ]
==>  PASS / FAIL

=== bus2 6-motor §7 手动 ClearEStop ===
rm latch 后重启第一 status: <粘贴 'state=Safe guards.estop=false'>
mock_s1 12s 期间右臂动作: [ 完整 ballet / 卡 Safe / 部分抖动 ]
==>  PASS / FAIL
```

我会基于这份回执决定下一步：
- 全 PASS → task #15 Stage 1 完结；扩 mock_s1 加 mask 模式接 Stage 2（base 3 wheel）。
- 任意 FAIL → 锁 root cause，可能要回 robot-s0 / feetech adapter
  改代码再上 Orin。

---

## 附 A：常见排查点

| 症状 | 怀疑点 |
|---|---|
| factory 报 HardRT 不是 SerialRT | feetech adapter `info().rt_class` 没改成 SerialRT，或 factory `build_serial_rt` override 漏；本仓库 master 已修，确认 git log |
| `motors 6 \| ids []`（空 ids） | toml `motor_ids` 字段拼写错；或 fallback `1..=6` 应当生效，看 factory 日志 |
| `calibration=none` | toml 里 `calibration_paths` 路径错；用 `ls /home/loosqk/repo/dimensional-applications/calibration/{right_arm,base}.json` 确认 |
| 启动期电机跳 | mock_s1 `--anchor-timeout-ms` 太小（< 500ms）或 daemon 启动慢；调到 1500ms |
| 摆动幅度异常大 | dimos calibration 的 raw_offset / 反向 / silent clamp 触发；先用 ±0.05 rad 试更小幅度，看是否仍异常 |
| 个别 motor 不动 | 该 motor ID 配置错或物理不通；feetech_poke 单 ping 隔离 |
| daemon 频繁报 read_state Err | bus2 RTT 异常 / motor 掉电；feetech_poke 跑一遍 ping 健康基线 |
| EStop 后 motor 仍 torque-on | safety_stop_fast 没生效或 broadcast 没到；feetech_poke 单 motor read Torque_Enable 看真值 |
| daemon `Permission denied (os error 13)` 启动 fatal | latch 文件 owner 跟当前 daemon user 不一致（如之前 sudo daemon 写 root-owner latch，现在 user daemon 读不到）；`sudo rm -f <latch> <latch>.bak` 重置 |
| mock_s1 / robotctl `ServiceInCorruptedState` | iceoryx2 service 元数据 owner 不一致或 stale；`sudo rm -rf /tmp/iceoryx2/nodes/* /tmp/iceoryx2/services/*` 重置 |
| robotctl watch 全程 "no sample yet" | iceoryx2 0.8.1 multi-sub attach race，**不阻断物理验证**（详 附 C.1）；workaround：用 daemon log + mock_s1 log + post-run motor pos 三方合验，watch 单跑（不与 mock_s1 并发）能拿 sample |
| feetech_poke "Device or resource busy" | daemon 占着 ttyACM1，**期望行为**；mid-run 不能并发读 motor，等 daemon 退出后再 poke（详 附 C.3）|

---

## 附 B：task #15 之前已落地的关键修复（git log）

```
60ba241 🐛 fix: task #15 latch 目录前置 —— 启动期 ensure_latch_parent + runbook 显式 install
dc45652 🐛 fix: task #15 阻断 1+2 —— EStop 触发即 fsync latch 落盘 + robotctl --duration-secs
bcee124 ✨ feat: task #15 真机 bus2 6-motor 准备 —— robotctl estop 子命令 + bus2 right_arm config + Orin runbook
cf38d5d ✅ test: task-30 v9 SerialRT 端到端 5 条门槛 + 修复 race / per-event guard
d89d255 ✨ feat: task-30 v9 M7 阶段 5 —— pub_thread 接 rt_ctrl.safety_state 串接 S0SafetyState wire
fda1450 ✨ feat: task-30 v9 M7 阶段 4 第二步 —— SerialRT 路径 Factory + s0_daemon dispatch 接通
4692d10 ✨ feat: task-30 v9 M7 阶段 4 第一步 —— s0_daemon dispatch + ipc_thread 接 ipc_tick
de67bb7 ✨ feat: task-30 v9 M7 阶段 3 —— ipc_tick errata 三步式 + §0.5 三测红灯门槛
```

2026-04-26 Orin 真机**第一次跑完整 SerialRT 路径**（M7 接通 + 阻断 1 修复
后），不再像 task 28/29 那种"adapter 跑但 state_machine 不接通 enable"
的半截状态。

---

## 附 C：2026-04-26 实测暴露的 known-issue（不阻断 #15 但记账）

### C.1 iceoryx2 0.8.1 single-service multi-subscriber attach race

**复现**：daemon + 2 个 `robotctl watch`（**没** mock_s1，**没** sudo），
第一个 watch 拿 17 samples，第二个 watch **0 samples**。daemon →
mock_s1.anchor + watch 同跑也是 watch 0 samples（无论 watch 起得比
mock_s1 早还是晚）。

**影响**：runbook §4-§7 的"wire 字段三方合验"路径打折扣 —— `robotctl watch`
观测不到 wire `state` / `safety_state` / `fault_code`。**不影响**物理
正确性（control 链路不走 watch；watch 仅诊断观测）。

**workaround**：
- 物理验证靠 **daemon log + mock_s1 log + post-run motor pos** 三方合验
  （§3.x 实测 §3-§7 全过）
- watch 单独跑（不并发 mock_s1）能拿到 sample（17/8s @ 500ms）
- 需要并发观测 wire 字段时改用 `robotctl status`（一次性快照）多次手动 fire

**follow-up**：升 iceoryx2 0.9 / 0.10 看是否 fix；或 watch 用独立 service。
**当前 task #15 scope 外**。

### C.2 sudo ↔ user 模式切换的 ownership 残留

**踩到的具体路径**（2026-04-26 同 session 复现）：
1. `sudo daemon` 写 latch 文件 → root owner
2. 后续 `user daemon` `load_estop_latch` 读 → `Permission denied (os error 13)`
   → daemon 直接 fatal 退出
3. 此外 `sudo daemon` 创建的 `/tmp/iceoryx2/nodes/<uuid>/` 是 root
   owner，下次 user mock_s1/robotctl open service →
   `PublishSubscribeOpenError(ServiceInCorruptedState)`

**结论**：**不要**在同一台机器上 sudo / user 混跑 daemon。统一一种模式
（推荐全 user，详 §0.1）。

**workaround**（需要切换模式时）：
```bash
sudo rm -f <latch_path> <latch_path>.bak
sudo rm -rf /tmp/iceoryx2/nodes/* /tmp/iceoryx2/services/*
```

### C.3 mid-run motor 验证不可用

daemon 占着 `/dev/ttyACM1` 时，`feetech_poke read ...` 报
`Device or resource busy`。这是**期望行为**（串口一次只能一个进程开）。

**workaround**：baseline → post-shutdown 对比 motor `Present_Position` /
`Torque_Enable`；mid-run 状态靠 daemon log + mock_s1 log（cmd_seq 数 +
state= 字段）。
