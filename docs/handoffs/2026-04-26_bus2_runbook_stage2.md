# Handoff · 2026-04-26 · bus2 9-motor 全栈真机闭环 Runbook（Stage 2）

> **状态：⏳ 软件就绪，待 Orin 真机验证**。Stage 1（[6-motor right_arm](
> 2026-04-26_bus2_runbook.md)）已 PASS；Stage 2 在 Stage 1 之上加 base 三
> 个 wheel（ID 7-9）= 9 motor 全栈，让 SerialRT 多 motor 路径在更大组上
> 跑稳。本 runbook 与 Stage 1 互补，**只列差异**；通用步骤（环境清理 /
> 编译 / sudo↔user 模式 / latch 路径 / 排查表）请直接参考 Stage 1 runbook
> §0-§1 + 附 A / 附 C。

---

## 0. 跟 Stage 1 的差异（必读）

1. **配置换成** [`configs/xlerobot_bus2_full.toml`](
   ../../configs/xlerobot_bus2_full.toml)（9 motor / `right_arm_full_with_base`）。
2. **mock_s1 加 mask 模式**：`--joints 9 --frozen-joints 6,7,8`，让 base 三轮
   target 始终 = anchor 读到的当前位置，不叠加正弦。其它 6 个右臂关节正常
   ±5° ballet（amplitude 0.087 rad、frequency 0.25 Hz）。
3. **base wheel 必须先在位置模式**：dimos 平时把 base motor 的
   `Operating_Mode` 设到 1（wheel/velocity 模式），那种状态下 sync_write
   `Goal_Position` 会被解释为速度命令 → wheel 持续旋转 → 整车跑掉。
   **Stage 2 跑前必须先把 `Operating_Mode` 切到 0 + 复读断言**（详 §2.2）；
   `Operating_Mode` 在 EEPROM 区，写之前必须 `Torque_Enable=0` **且**
   `Lock=0`，少一步就静默写失败。
4. **启动期 6→9 motor 防飞**：`FeetechAdapter::enable(true)` 已经在 task
   29 落地 follow-current（sync_read Present → sync_write Goal 让 target
   追平 current 再上扭），9-motor 同款；mock_s1 anchor 也读到 9 个 bias。
5. **回归同款** Stage 1 §3-§7 验证项；新增"base wheel 不动"额外锁。

---

## 1. 编译

跟 Stage 1 同 build；本 stage 多了一个 mock_s1 mask 参数（`--frozen-joints`）
但仍然在 `target/release/mock_s1` 同一 binary 里，不需要重新 build：

```bash
# 已经按 Stage 1 编译过 release，跳过即可。
# 如果未编译：
cd ~/repo/robot/robot-system
git fetch && git checkout master && git pull --ff-only
source ~/.cargo/env
cargo build --release \
    -p robot-s0 --no-default-features --features xlerobot-real \
    -p robot-s1 -p robot-cli
cargo build --release --example feetech_poke -p robot-adapter-feetech --features real-hw
```

---

## 2. 真机准备

### 2.1 ping 9 motor（同 Stage 1 §2，**全 PASS** 才进 §2.2）

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00
for id in 1 2 3 4 5 6 7 8 9; do
    target/release/examples/feetech_poke "$BUS" "$id" ping
done
```

期望：9 行 `ping ok ... (Model_Number=STS3215 777)`。如果只有 1-6 通、7-9
不通，先排查物理连线 / 电源 / motor 自身，**不要进 §2.2**。

### 2.2 base wheel 强制位置模式（关键安全步骤）

dimos 在运行期把 base motor 切到 wheel/velocity 模式
（`Operating_Mode=1`，`Goal_Position` 被解释成 `Goal_Speed`）；service
stop 不切回。本步骤把 7、8、9 三个 motor 强制写 `Operating_Mode=0`（位
置模式）+ 复读断言。

> **寄存器名硬要求**：必须用 **`Operating_Mode`**（addr 33）。
> `feetech_poke` 是精确查表（`crates/robot-adapters/feetech/src/registers.rs::STS3215_CONTROL_TABLE`），
> 写错成 `Mode` / `OperatingMode` / `mode` 都会直接返
> `unknown register`。
>
> **EEPROM 写保护硬要求**：`Operating_Mode` 在 EEPROM 区
> （`EEPROM_REGISTERS`，同文件）。STS3215 EEPROM 写要求 `Torque_Enable=0`
> **且** `Lock=0`（解锁）。少了 `Lock=0` 这步**写包会被 motor 静默拒**——
> 看不到错误，但 readback 还是旧值，ballet 一跑车就跑掉。所以下面序列
> 三步缺一不可。

完整切换序列（每个 base motor 都做一次）：

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00

# ── 1) 读当前 Operating_Mode（baseline）──
for id in 7 8 9; do
    echo -n "motor $id Operating_Mode = "
    target/release/examples/feetech_poke "$BUS" "$id" read Operating_Mode
done
# 期望：3 行值。全 = 0 可直接跳 §2.3；有 1 / 其它值 → 进 step 2。

# ── 2) 强制切到位置模式（含 EEPROM 解锁 + 重锁）──
for id in 7 8 9; do
    # a. 必须先断扭（带扭改 EEPROM 静默失败）
    target/release/examples/feetech_poke "$BUS" "$id" write Torque_Enable 0
    # b. EEPROM 解锁（少这步 write 直接被吞）
    target/release/examples/feetech_poke "$BUS" "$id" write Lock 0
    # c. 切位置模式
    target/release/examples/feetech_poke "$BUS" "$id" write Operating_Mode 0
    # d. 重锁 EEPROM（防止后续 daemon 操作误改其它 EEPROM 寄存器）
    target/release/examples/feetech_poke "$BUS" "$id" write Lock 1
done

# ── 3) 复读断言 Operating_Mode == 0 ──
all_ok=true
for id in 7 8 9; do
    echo -n "motor $id Operating_Mode = "
    out=$(target/release/examples/feetech_poke "$BUS" "$id" read Operating_Mode)
    echo "$out"
    case "$out" in *"= 0"*|*"=0"*) ;; *) all_ok=false ;; esac
done
$all_ok && echo "OK: all base motors at position mode" \
        || { echo "FAIL: 至少一个 base motor Operating_Mode != 0；"\
             "停下排查（最常见原因：Lock 没解、Torque 没断、bus 通信丢包）"; exit 1; }
```

**为什么 step 2 序列必须按 a→b→c→d**：a 和 b 顺序可换（都是写 SRAM 区
+ 一个 EEPROM 锁位），但 c（写 EEPROM）必须在 a + b **都已落地**之后；d
重锁是为了让"daemon 后续 enable(true) → write `Torque_Enable=1`" 这条
SRAM 写不会因为 Lock 还开着而被怀疑（Torque_Enable 在 SRAM 区不受 Lock
影响，但留 Lock=0 增加误改其它 EEPROM 寄存器的风险，所以 §2.2 完成立
即重锁）。

**断言失败的最常见原因**：
- `Lock 0` 写之前忘了 `Torque_Enable 0`（部分批次 motor 会拒）
- `Lock 0` 跟 `Operating_Mode 0` 之间 bus 丢包导致前一条没生效
- 写错成 `Mode` 寄存器名 → `unknown register` 直接 fail（命令行该报错）

**跑完 Stage 2 后**：dimos 启动会自己 `Operating_Mode=1` 把 base 切回
wheel 模式，**不需要**手动恢复。

### 2.3 base wheel `Torque_Enable` baseline + `Present_Position` baseline

跑 ballet 前记录 9 个 motor 的 `Torque_Enable` + `Present_Position`，方
便 §3-§5 对比"是否真的全 0 / 正确发生 enable / base 不动"。

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00

for id in 1 2 3 4 5 6 7 8 9; do
    echo "=== motor $id baseline ==="
    target/release/examples/feetech_poke "$BUS" "$id" read Torque_Enable
    target/release/examples/feetech_poke "$BUS" "$id" read Present_Position
done | tee logs/bus2_stage2_baseline.log
```

预期：
- 1-6 `Torque_Enable=1`（dimos 退出留扭 baseline，跟 Stage 1 一致）
- 7-9 `Torque_Enable=1` 或 `0`，看 §2.2 操作 + dimos 退出时的状态
- 9 个 `Present_Position` raw 值各异，**记下来**给 §6 比较

---

## 3. Smoke：s0_daemon 不发命令 5s

```bash
target/release/s0_daemon \
    --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base \
    --config configs/xlerobot_bus2_full.toml \
    --latch-path logs/estop_latch_bus2_full \
    --smoke-duration 5s 2>&1 | tee logs/bus2_stage2_smoke.log
```

期望日志：
```
feetech factory (SerialRT): built 'right_arm_full_with_base' adapter on
  /dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00
  (motors 9 | ids [1, 2, 3, 4, 5, 6, 7, 8, 9] | calibration=loaded)
heartbeat: state=Ready  guards.estop=false  guards.hb_ok=false  t=2.00...
heartbeat: state=Ready  guards.estop=false  guards.hb_ok=false  t=4.00...
smoke duration elapsed → Event::Stop
```

**验证项**：
1. ✅ `motors 9 | ids [1, 2, 3, 4, 5, 6, 7, 8, 9]` —— 9-motor adapter 真的
   build 出来了。
2. ✅ `calibration=loaded`。
3. ✅ state=Ready 全程。
4. ✅ 物理：右臂 + 整车（含 base）全程不动。daemon 仅 connect()，**不**
   下发 enable / safety_stop。
5. ✅ clean shutdown。

---

## 4. Ballet 14s（关键闭环）

终端 A（s0_daemon）：

```bash
target/release/s0_daemon \
    --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base \
    --config configs/xlerobot_bus2_full.toml \
    --latch-path logs/estop_latch_bus2_full \
    --tick-hz 50 \
    --smoke-duration 14s 2>&1 | tee logs/bus2_stage2_ballet_s0.log
```

终端 B（mock_s1，**注意 `--joints 9` + `--frozen-joints 6,7,8`**）：

```bash
sleep 1
target/release/mock_s1 \
    --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base \
    --joints 9 \
    --amplitude 0.087 \
    --frequency 0.25 \
    --anchor-timeout-ms 1000 \
    --frozen-joints 6,7,8 \
    --duration 12s 2>&1 | tee logs/bus2_stage2_ballet_s1.log
```

> **注意 frozen-joints 是 0-based**。joint 索引 6/7/8 对应 `joints` 数组
> 的第 7/8/9 个，也就是 `base_left_wheel` / `base_back_wheel` /
> `base_right_wheel`。右臂 6 个关节是索引 0..=5，会正常摆。

终端 C（robotctl watch，可选）：

```bash
target/release/robotctl watch \
    --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base \
    --interval-ms 250 \
    --duration-secs 13 2>&1 | tee logs/bus2_stage2_ballet_status.log
```

**注意**：本批次同时修复了 heartbeat QoS 错用 `S1CmdVel`
（`max_subscribers=1`）的 bug，新增 `TopicClass::Heartbeat`
（`max_subscribers=8` + `safe_overflow=true`）。
Stage 1 §5.1 known-issue（"watch 第二个 sub 拿不到 sample"）已根除：
本 stage 应该可以**多个 watch 并跑**正常拿样本，验证一下：

```bash
# Stage 2 顺手回归 stage-1 watch QoS 修复：起 2 个 watch 看是否都拿到 sample
# 终端 C1
target/release/robotctl watch --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base --interval-ms 200 --duration-secs 10 \
    2>&1 | tee logs/bus2_stage2_watch_a.log &
# 终端 C2（同时跑）
target/release/robotctl watch --robot-id xlerobot_bus2_full \
    --hardware-id right_arm_full_with_base --interval-ms 200 --duration-secs 10 \
    2>&1 | tee logs/bus2_stage2_watch_b.log &
wait
```

**验证项**：
1. ✅ daemon log 出现 `state=Enabled.Active`（≥ 8 秒持续）。
2. ✅ mock_s1 anchor 行：`anchor: bias = [b0, b1, b2, b3, b4, b5, b6, b7, b8]`
   —— 9 个 bias，对应 9 motor 当前 Present_Position。
3. ✅ 物理：**右臂** 6 关节同步正弦摆动 ±5°；**底盘** 三个 wheel 全程几乎
   不动（肉眼检查不到运动；微动 < ±2°）。
4. ✅ 两个 watch 都拿到 sample（**watch QoS 修复回归**），不再像 Stage 1
   §5.1 那样第二个 watch 0 sample。
5. ✅ daemon clean shutdown。

**安全注意（同 Stage 1 §4 ⚠️）**：ballet 退出后 9 motor 仍处 enable(true) 上
扭状态。**接触硬件前必须** ① 跑 §5 EStop 触发 broadcast 断扭，**或** ②
手动 `feetech_poke write Torque_Enable 0` 逐 motor + readback 确认，**或**
③ 直接物理切电源。

---

## 5. EStop 真机端到端

跟 Stage 1 §5 同款，只是 robot_id / hardware_id / latch path 换：

```bash
# 终端 A（daemon）：参数同 §4
# 终端 B（mock_s1）：参数同 §4，含 --frozen-joints 6,7,8
# 终端 C（robotctl watch）：参数同 §4
# 终端 D（estop trigger，约 t=6s 触发）：
sleep 6
target/release/robotctl estop \
    --robot-id xlerobot_bus2_full \
    --reason "task15-stage2-bus2-9motor" 2>&1 | tee logs/bus2_stage2_estop_trigger.log
```

立即（< 100 ms）读 latch 文件：

```bash
sudo cat logs/estop_latch_bus2_full     # 期望：1
sudo cat logs/estop_latch_bus2_full.bak # 期望：1
```

post-shutdown 全 9 motor 断扭检查：

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00
for id in 1 2 3 4 5 6 7 8 9; do
    target/release/examples/feetech_poke "$BUS" "$id" read Torque_Enable
done
```

**验证项**（Stage 1 §5 + base 扩展）：
1. ✅ EStop 之后 < 1s 内右臂 + base 全部物理软掉（手指捏 base wheel 应该
   能轻松转动，证明 broadcast `Torque_Enable=0` 真覆盖了 base motor）。
2. ✅ wire `state=Safe`, `guards.estop_latched=true`。
3. ✅ wire `S0SafetyState=TorqueOffConfirmed`。
4. ✅ post-shutdown 9 motor `Torque_Enable=0` 全 0（**不只是 1-6，连
   7-8-9 也 = 0**）。
5. ✅ latch primary + bak 双文件 = "1"。

---

## 6. base 不动验证（Stage 2 独有锁测）

ballet + EStop 全跑完后，post-shutdown 读 base motor 的 Present_Position
跟 §2.3 baseline 对比。base wheel **frozen** 期间应该几乎不动：

```bash
BUS=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00
echo "=== motor post Present_Position ==="
for id in 1 2 3 4 5 6 7 8 9; do
    echo "motor $id:"
    target/release/examples/feetech_poke "$BUS" "$id" read Present_Position
done | tee logs/bus2_stage2_post.log

# 跟 baseline 对比
diff <(grep -A1 "^motor [1-9]:" logs/bus2_stage2_baseline.log | grep Present_Position) \
     <(grep -A1 "^motor [1-9]:" logs/bus2_stage2_post.log      | grep Present_Position)
```

**验证项**：
- motor 1-5 / wrist_roll：Δ raw 在 -10..+50 范围（hold drift + 正弦最后帧
  phase 决定的快照，跟 Stage 1 实测一致）。
- motor 6 (gripper)：Δ raw 在 -10..+50 范围。
- **motor 7-9 (base wheel)：|Δ raw| < 30**（≈ 2.6°，frozen 模式 + EStop
  断扭后保持几乎不动）。

如果 base motor Δ raw > 100：要么 §2.2 `Operating_Mode` 切换没生效
（`Goal_Position` 被解释为速度命令；最常见原因是 EEPROM 解锁序列漏了
`Lock=0`，写包被静默拒），要么 mock_s1 没真把 base joint frozen。停下
排查：
- 检查 mock_s1 启动 banner 的 `frozen_joints=[6, 7, 8]`（task-32 tracing
  field）。
- 重读 base motor `Operating_Mode`：应当 = 0；不是 0 → 回 §2.2 重做完整
  序列（`Torque_Enable=0` → `Lock=0` → `Operating_Mode=0` → readback →
  `Lock=1`），不能跳步。

---

## 7. latch 重启 / 删 latch 恢复 / 第二次 EStop

跟 Stage 1 §6 / §7 完全同款，只是把 robot_id / latch path 换成
`xlerobot_bus2_full` / `logs/estop_latch_bus2_full*`。验证项不变。

---

## 8. 报回回执模板

```
=== bus2 9-motor §2.1 ping ===
9 motor 全 PASS / 部分 fail（列出 fail ID）
==>  PASS / FAIL

=== bus2 9-motor §2.2 base Operating_Mode 切换 ===
baseline Operating_Mode: motor 7=<v> 8=<v> 9=<v>
切换后 Operating_Mode: motor 7=<v> 8=<v> 9=<v>（必须 = 0）
切换序列是否完整: Torque_Enable=0 + Lock=0 + Operating_Mode=0 + readback + Lock=1 [完整 / 跳步]
==>  PASS / FAIL

=== bus2 9-motor §3 Smoke ===
factory: <粘贴 'feetech factory (SerialRT) ... (motors 9 ...)' 那行>
heartbeat: <粘贴 2 行 t=2.0 / 4.0 的 'state=Ready ...'>
物理: 右臂 + base [完全不动 / 抖动（具体描述）]
==>  PASS / FAIL

=== bus2 9-motor §4 Ballet ===
mock_s1 anchor 行: <粘贴 'anchor: bias = [...]'，必须 9 个 bias>
mock_s1 frozen_joints field: <粘贴 'frozen_joints=[6, 7, 8]'>
state_changes 计数: <数值>
两个 watch 各拿 sample 数: A=<n>, B=<m>（必须都 > 0）
物理: 右臂摆动 [同步 ±5° / 异常] + base [不动 / 微动 / 跑掉]
==>  PASS / FAIL

=== bus2 9-motor §5 EStop ===
estop 触发时刻: t=<秒>
软掉延迟: <估算 ms>
post latch primary/bak: <值>
post 9 motor Torque_Enable: <列出 9 行>（必须全 0）
==>  PASS / FAIL

=== bus2 9-motor §6 base 不动锁测 ===
motor 7 ΔPos: <raw>（必须 |Δ| < 30）
motor 8 ΔPos: <raw>
motor 9 ΔPos: <raw>
==>  PASS / FAIL

=== bus2 9-motor §7 latch 重启 / 删 latch / 第二次 EStop ===
（同 Stage 1 §6+§7 模板，只换 path）
==>  PASS / FAIL
```

---

## 附 A：与 Stage 1 共享的排查表

复用 [Stage 1 runbook 附 A](2026-04-26_bus2_runbook.md#附-a常见排查点)
的所有条目。Stage 2 新增条目：

| 症状 | 怀疑点 |
|---|---|
| factory `motors 6` 不是 `motors 9` | 启动用了 Stage 1 配置；检查 `--config` 路径 |
| base wheel 跑掉 | §2.2 `Operating_Mode` 切换没做 / 没生效（最常见：EEPROM `Lock=0` 漏写 → 写包被吞）；停 daemon → 完整跑 §2.2 序列（断扭 + 解锁 + 写 + readback + 重锁） |
| ballet 期间 base motor Δ raw > 1000 | mock_s1 没传 `--frozen-joints 6,7,8`；检查 mock_s1 启动 banner |
| `--frozen-joints: index N out of range` | mock_s1 `--joints` 给了 < 9，跟 frozen 索引冲突；改成 `--joints 9` |
| post-shutdown motor 7-9 仍 `Torque_Enable=1` | §5 EStop 后 base wheel 没断扭；可能是 broadcast 没生效（实测应当全 0）；feetech_poke 单读多次确认 |

## 附 B：Stage 2 软件层依赖（git log 上游）

```
（本 commit）  feat: task #15 Stage 2 9-motor 全栈 + mock_s1 mask 模式 + watch QoS 修复
60ba241       fix: task #15 latch 目录前置
dc45652       fix: task #15 阻断 1+2 EStop 落盘
bcee124       feat: task #15 真机 bus2 6-motor 准备（Stage 1）
```
