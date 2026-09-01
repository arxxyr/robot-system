# dimos → robot-system 交接 runbook（v0.5 候选 E 段(9)）

> **范围**：本 runbook 只覆盖**部署件**——config 安装、unit ×7 装载、
> handoff transaction、guard 启动链、失败与人工恢复、旧栈处置。
> **真机受监督 demo 的执行步序在段(10-demo)**（Spec P.2.10），本文件不代替
> 它，也不构成"可以上电动起来"的许可。
>
> 收口宣称唯一口径 = **"机器人控制执行面受监督功能 demo 跑通"**；
> **不得**宣称生产替换 / 无人值守 / 硬件 fail-safe 完成（D15 仍 OPEN）。

---

## 0. 交接的安全前提（先读，别跳）

arm-only S0 **不驱动也不能 EStop** head / base 电机；旧栈退出只 best-effort
断扭并吞掉失败（`_manipulator_base.py` disconnect 逐电机 `Torque_Enable=0`
包在 `except: pass`；`bus2_base.py` 轮子 `Goal_Velocity=0` 同款）。因此切换
窗口内 head / wheels 的扭矩与速度状态**未知**。

更要命的是 `auto_enable = false` **只防新 S0 自动上扭，不消除崩溃残留扭矩**
——Feetech `connect()` 只建连接 / 翻译 id / 建缓存，**不写 `Torque_Enable=0`**。
S0 在 Enabled 状态崩溃后，本臂 ID 1-6 在新 S0 起来时仍保持上次带扭状态。

### 0.1 arm ↔ config 必须锁在同一条 D2 行上

`--arm` 决定**安全写序**（左 `[T1..T8]` / 右 `[T1..T6, V7,T7, V8,T8, V9,T9]`），
config 决定**打开哪条物理串口**。两者不绑定时，`--arm left --config
…right_arm.toml` 会打开 bus2 却跑左侧写序——**干扰正在跑的右臂，并且漏掉
wheel ID 9 的零速+断扭**，全程没有任何运行期信号。

因此 guard / `preflight-arm` / `preflight-handoff` 在**打开串口之前**按 D2
身份表逐字校验 config：`[robot].id` / `[[hardware]].id` / `adapter` /
**完整 by-id 串口路径（整串相等，不是子串匹配）** / `motor_ids` / `joints` /
`auto_enable = false` / **`calibration_paths`（恰好一项、绝对路径、文件名 ==
本臂标定文件）**；guard 另外校验转发给 `s0_daemon` 的 `--robot-id` /
`--hardware-id` 与 D2 一致。任一项不符 → 退 86，**不消耗连败预算**（配置错误
每次重试必然重现）。

| | 左臂 | 右臂 |
|---|---|---|
| `robot_id` | `xlerobot_left` | `xlerobot_right` |
| `hardware_id` | `left_arm` | `right_arm` |
| 串口（完整 by-id） | `/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C121427-if00` | `…_5A7C122340-if00` |
| `calibration_paths` | 恰好一项 `…/left_arm.json` | 恰好一项 `…/right_arm.json` |

> **为什么串口要整串比**：`contains("5A7C121427")` 会放行 `-if01` 这类伪后缀
> 和 `5A7C1214270` 这类更长的伪串号，两者都指向另一个设备节点。
>
> **为什么标定必须校验**：`calibration_paths` 住 `[[hardware]]` 的 extras，
> `s0_daemon` 对「缺失 / 非数组 / 元素非字符串」一律静默折叠成空数组，而空
> 数组在 feetech factory 里等价于「无标定照常启动」——整条链不会报任何错。

所以启动链是 **fail-closed** 的：任何一条安全写或 readback 失败 → 该臂新栈
**不起**，旧 unit 未 disable、未删除，**是否回启旧栈与残余风险由人工判断**
（硬件 I/O 已失败的场景**不承诺**旧栈必然安全恢复）。

---

## 1. 安装（一次性）

### 1.1 config → `/etc/robot/`（R10-1 三处引用同一绝对路径）

```bash
sudo install -D -m 0644 configs/xlerobot_left_arm.toml  /etc/robot/xlerobot_left_arm.toml
sudo install -D -m 0644 configs/xlerobot_right_arm.toml /etc/robot/xlerobot_right_arm.toml
sudo install -D -m 0644 configs/rpc_roles.toml          /etc/robot/rpc_roles.toml
```

仓库文件名与安装路径**一一对应**（不改名）。这三处引用同一份绝对路径：

| # | 引用点 | 字面 |
|---|---|---|
| 1 | handoff unit | `preflight-handoff --left-config /etc/robot/xlerobot_left_arm.toml --right-config /etc/robot/xlerobot_right_arm.toml` |
| 2 | arm S0 unit | `guard --config /etc/robot/xlerobot_<side>_arm.toml` |
| 3 | `s0_daemon` argv | guard **自动追加** `--config <同一路径>` |

**`--` 之后的 s0 args 绝不能自带 `--config`**——重复即配置错误，guard 退 86
（结构性保证 guard 与 `s0_daemon` 不会引用两份配置）。

### 1.2 latch 目录写权限

```bash
sudo install -d -o loosqk -g loosqk -m 0755 /var/lib/robot/xlerobot_left
sudo install -d -o loosqk -g loosqk -m 0755 /var/lib/robot/xlerobot_right
```

latch 文件本身**不要手动创建**：guard 第 ④ 步 `provision` 会按三分支写入
（双缺失 → 写 `1\n`（**latched**）双份 —— 换 robot_id 不得借机丢掉 EStop
记忆）。首启后状态链 =
`latched Safe → ClearEStop（maintainer+）→ 仍 Safe → SetEnabled(true) → Ready`。

### 1.3 build（Orin 本地）

```bash
cargo build --release --workspace
cargo build --release -p robot-s0 --bin xlerobot_deploy --features xlerobot-real
cargo build --release -p robot-s0 --bin s0_daemon       --features xlerobot-real
```

`xlerobot_deploy` 的 `required-features = ["xlerobot-real"]`：不带该 feature
时 cargo **静默跳过**这个 bin，装 unit 前先确认
`target/release/xlerobot_deploy` 真的存在。

### 1.4 unit ×7

```bash
sudo install -m 0644 scripts/systemd/xlerobot-handoff.service \
                     scripts/systemd/xlerobot-s0-left-arm.service \
                     scripts/systemd/xlerobot-s0-right-arm.service \
                     scripts/systemd/xlerobot-s1-left.service \
                     scripts/systemd/xlerobot-s1-right.service \
                     scripts/systemd/xlerobot-gateway-left.service \
                     scripts/systemd/xlerobot-gateway-right.service \
                     /etc/systemd/system/
sudo systemctl daemon-reload
```

unit 里的 `User=loosqk` / `WorkingDirectory=` / 二进制绝对路径按实际部署根
调整；**config 的三处 `/etc/robot/...` 字面不要改**（改要三处一起改，并同步
`crates/robot-s0/tests/deploy_units.rs` 的一致性锁）。

---

## 2. 依赖拓扑（谁拉起谁）

```
xlerobot-handoff.service            Type=oneshot + RemainAfterExit=yes
  ├─ Conflicts/After  dimos ×2（coordinator / web）+ 旧 full S0 ×2
  └─ ExecStart        preflight-handoff（全 17 电机 / 20 条写）
        ▲ Requires + After
        │
xlerobot-s0-<side>-arm.service      ExecStart = guard（六步）→ exec s0_daemon
        ▲ Requires + After
        ├── xlerobot-s1-<side>.service
        └── xlerobot-gateway-<side>.service   （另 Wants + After 同臂 S1）
```

- **handoff 是旧栈停止顺序的第一责任人**：conflict 目标的 stop job 按 `After`
  排在它 start 之前完成。
- `RemainAfterExit=yes` → 同一 boot 内启动**第二条臂时 handoff 不重跑**
  （不去碰第一臂已占的串口）；reboot 后自动重新验证（断电后扭矩本就复位，
  重验无害且 fail-closed）。
- 只启动一条臂也**先保全部 17 电机安全**；随后该臂 S0 的 enable 序列只重新
  上扭它自己的 6 个电机。**单臂运行期间另一臂电机保持 handoff 断扭态。**
- watchdog 重启只重跑本臂 guard（handoff 因 `RemainAfterExit` 不重跑）。

启动：

```bash
sudo systemctl start xlerobot-gateway-left.service   # 会自动拉起 handoff → S0 → S1
```

---

## 3. handoff transaction 做了什么

`xlerobot_deploy preflight-handoff` 对**两条 bus 全部 17 个电机**执行：

| bus | 电机 | 安全写 |
|---|---|---|
| bus1（左） | ID 1-8（6 臂 + 2 head） | `Torque_Enable = 0` —— 8 条 |
| bus2（右） | ID 1-6（6 臂） | `Torque_Enable = 0` —— 6 条 |
| bus2（右） | ID 7-9（3 wheels） | 先 `Goal_Velocity = 0` **再** `Torque_Enable = 0` —— 6 条 |

合计 **20 条安全写**，写序严格 = 左 `[T1..T8]`、右
`[T1..T6, V7,T7, V8,T8, V9,T9]`（**每轮零速先于同轮断扭**），随后**逐项
readback** 断言值为 0。

- **单项失败不中断**：继续处理其余电机，末尾聚合；
- **一侧串口构造失败不阻断另一侧**：失败侧记 `Connect{arm}` 并跳过整条写序，
  另一侧照常完整执行；
- 任一失败 → 整体 FAIL（exit 70）→ handoff unit failed → 两个 arm S0 unit
  因 `Requires=` 不会启动。

单独跑（诊断用，不改变 unit 状态）：

```bash
target/release/xlerobot_deploy preflight-handoff \
    --left-config /etc/robot/xlerobot_left_arm.toml \
    --right-config /etc/robot/xlerobot_right_arm.toml
target/release/xlerobot_deploy preflight-arm --arm left \
    --config /etc/robot/xlerobot_left_arm.toml
```

---

## 4. guard 六步与退出码

`ExecStart` 是 **guard**，不是 `s0_daemon`——`RestartPreventExitStatus=86`
只作用于主进程，pre 步放 `ExecStartPre=` 会让停止态失效。

1. 持 `/run/robot-system/preflight/<robot_id>.lock`（**稳定 inode**，永不
   rename / 删除），读 `<robot_id>.count`；`== 5` → **退 86**；
2. **在任何 preflight / provision 之前**原子写入 `n+1`（写失败 → 86）；
3. 本 bus 全量 ensure-safe（左 8 条写 / 右 12 条写 + 各自 readback）；
4. latch provision（三分支 + 统一 parent-dir fsync）；
5. 两步全绿 → **清零计数**（清零失败 → 86）；
6. 显式关锁 → `exec s0_daemon`（自动追加 `--config`）。

计数语义 = **未成功 guard 尝试数**（pre-increment）：guard 在慢 preflight 中
panic / 被 SIGKILL 也保留已递增值，下次启动接着累计。

| 退出码 | 含义 | systemd 行为 |
|---|---|---|
| `0` | 正常（guard 成功 = exec 不返回，之后由 `s0_daemon` 决定） | — |
| `2` | CLI 参数错误 | `Restart=on-failure` 拉起（必然重现，会走到限流） |
| `70` | preflight 失败（连败未满 5） | `RestartSec=2s` 重试 |
| `71` | provision 失败（连败未满 5） | `RestartSec=2s` 重试 |
| `86` | **停止态**：预算耗尽 / 计数 I/O 异常 / 锁冲突 / 配置级 exec 前置失败（含 D2 身份不符、`--` 后带 `--config`、`--latch-path` 重复或等号写法） | `RestartPreventExitStatus=86` → **停止自动重启，等人** |

两档预算彼此独立：

- **运行期崩溃 / watchdog 风暴** → unit 级 `StartLimitIntervalSec=60s` +
  `StartLimitBurst=5`；
- **preflight/provision 连败** → guard 自管、**与墙钟无关**的计数（单次 pre
  最坏 ~12 s，会滚出 60 s 窗口，unit 级限流抓不住慢失败循环）。

### 4.1 两个不能改的 unit 数值

- **`TimeoutStartSec=infinity`**（handoff + 两条 arm S0）：D13 明确"给
  preflight 定义可证明总时限"这一备选**未采**——任何有限值都可能在"部分电机
  已写、其余尚未处理"时把进程杀掉，留下一条半安全的 bus 且没有聚合报告。
  单次 preflight 最坏 ~12 s/bus 本身有界；真卡死由人工介入。
- **生产 `WatchdogSec`**：S1 / gateway = **5s**（Spec D12 拍板），S0 = 2s；
  `scripts/systemd/*-freeze-test.service` 测试模板保持 2s。daemon 侧 ping
  周期恒 = systemd 注入的 `WATCHDOG_USEC / 2`，代码里没有硬编码。

`RuntimeDirectory=robot-system/preflight` + `RuntimeDirectoryMode=0750` +
`RuntimeDirectoryPreserve=yes` 四态：**首次启动自动创建 / restart 保留 /
手工 stop-start 保留 / reboot 清零**（/run 是 tmpfs，计数**不**具备掉电
持久性；reboot 后重新执行 handoff 与 guard）。

---

## 5. 人工恢复（退 86 之后）

```bash
# 1) 先看是哪一类
journalctl -u xlerobot-s0-left-arm.service -n 200 --no-pager
systemctl show -p ExecMainStatus xlerobot-s0-left-arm.service

# 2) 修硬件 / 修配置（串口、供电、config 路径…）

# 3) 清连败计数（**只清 .count，绝不动 .lock**）
sudo rm -f /run/robot-system/preflight/xlerobot_left.count

# 4) 重启
sudo systemctl start xlerobot-s0-left-arm.service
#    若同时撞上 unit 级 60s/5 限流，另需：
sudo systemctl reset-failed xlerobot-s0-left-arm.service
```

> `.lock` 文件**永不删除 / 永不 rename**：`flock` 绑定 inode，把锁打在会被
> rename 的文件上时，竞争进程能同时"持锁"。

---

## 6. 旧栈处置（D-BI3 r5 现行口径，2026-09-01）

- 允许：为释放硬件而 `systemctl stop`（handoff 的 `Conflicts=` 也会自动做
  这件事）。
- **r5（2026-09-01，用户拍板）：coordinator / web 已 `systemctl disable`**
  （仅关自启，unit 文件保留；触发事实 = isolcpus 重启后旧栈 enabled 自启
  占双串口）。回滚 = 显式 `systemctl enable`（如需自启）+ `start`。
- **仍然禁止**：删除 unit / `mask` / 移除 LCM 等运行时——即任何破坏
  回滚路径的动作。
- 正式退役、删除、LCM 随仓退出——**全部等 post-E 生产安全候选闭环**
  （决策单 D-BI3 r3 时点不变；r5 只解除 disable 禁令，见 CHANGELOG
  2026-09-01 + 决策单 r5 修订块）。

旧栈 unit：`xlerobot-coordinator.service` / `xlerobot-web.service` /
`xlerobot-s0-bus1.service` / `xlerobot-s0-bus2.service`。

---

## 7. 应急协议（Enabled 期间，权威文本见 Spec P.2.10）

**Enabled 期间任一 S0 进程死亡 / 重启 → 立即人工切断总电源并中止本轮。**

**同款人工协议扩到整条链（D17 已登记的 client 死亡风险 + 通信韧性评估
§6.1）**：Enabled 期间 `supervised_demo`（验收 bin）/ gateway / S1 任一
**死亡或挂起** → 同样立即人工切断总电源并中止本轮——协议侧的 cancel /
双 EStop 依赖 client 与 gateway 存活，client 死了没有任何软件路径会去收尾；
跨臂也没有 daemon 级联锁（一臂 Fault 不会自动断另一臂的扭）。

**不得只依赖 EStop 弱 Ack**——S0-dead 时 Ack = `published_at_ns`，**不证明
事件已被消费**。

恢复 = 断电确认 → 排查根因 → 重新走安全边界 checklist 从步 1 起
（checklist 与 demo 步序在段(10-demo)，本 runbook 不代替）。

---

## 8. 相关文件

| 文件 | 作用 |
|---|---|
| `scripts/systemd/xlerobot-handoff.service` | 全 17 电机交接（唯一入口） |
| `scripts/systemd/xlerobot-s0-{left,right}-arm.service` | guard → `exec s0_daemon` |
| `scripts/systemd/xlerobot-s1-{left,right}.service` | S1 coordinator（硬依赖同臂 S0） |
| `scripts/systemd/xlerobot-gateway-{left,right}.service` | gRPC gateway（50051 / 50052） |
| `scripts/systemd/README.md` | 旧 full-bus unit 的安装 / 验证说明（保留） |
| `crates/robot-s0/src/deploy/` | identity / guard / preflight / provision / counter 实现 |
| `crates/robot-s0/tests/deploy_units.rs` | unit 指令文本 + 三处 config 路径一致锁 |
| `crates/robot-s0/tests/deploy_guard_kill.rs` | pre-increment 崩溃注入（真 SIGKILL） |

---

## 9. 受监督 demo（段(10-demo)；权威步序 = Spec P.2.10）

> **本节只是命令面。执行门槛 = AC-E12 安全边界五条 + 操作者在场 + 明确
> 放行**；`supervised_demo` 的上扭 phase 都要求 `--ack-safety-checklist`，
> 不带即打印五条并退 2、绝不 `SetEnabled`。

```bash
# 0) demo 驱动件（PX-45；required-features 未满足时 cargo 静默跳过该 bin）
cargo build --release -p robot-tests --features supervised-demo --bin supervised_demo

# 1) 证据基线时刻（5c 取证窗起点）——在起臂之前记录一次：
DEMO_T0=$(date -Is)

#    起目标臂（handoff transaction 自动链：全 17 电机 ensure-safe → guard →
#    s0 → s1 → gateway）。⚠ 物理效应：旧栈被 Conflicts 停掉 + 全部电机断扭
#    ——若此刻有臂被扭矩撑着，会失力下垂。操作者必须在场。
sudo systemctl start xlerobot-gateway-left.service
sudo systemctl start xlerobot-gateway-right.service

# 2) 读状态（只读；不上扭）。附 robotctl 旁路证据（D10）。
ROBOT_TOKEN=operator-fake-token-001 \
  ./target/release/supervised_demo --phase read-state

# 3) 首启 latch 是 latched（guard provision 写 1\n，D2 状态链）：
#    ClearEStop（maintainer+）后仍 Safe，等 SetEnabled。左右各一次。
ROBOT_TOKEN=maintainer-fake-token-001 ./target/release/robotctl clear-estop \
  --confirmation CONFIRM_CLEAR_ESTOP --gateway-addr 127.0.0.1:50051 \
  --audit-reason "demo first-boot latch clear"
ROBOT_TOKEN=maintainer-fake-token-001 ./target/release/robotctl clear-estop \
  --confirmation CONFIRM_CLEAR_ESTOP --gateway-addr 127.0.0.1:50052 \
  --audit-reason "demo first-boot latch clear"

# 4) 安全边界五条逐条确认并记录（证据 docs）→ 单臂 → 双臂 → 双夹爪。
#    每个 phase 独立调用 = operator 显式；phase 内 SetEnabled(true)，
#    phase 末 SetEnabled(false)。
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase single-arm --side left  --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase single-arm --side right --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase dual-arm  --ack-safety-checklist
ROBOT_TOKEN=operator-fake-token-001 ./target/release/supervised_demo \
  --phase grippers  --ack-safety-checklist

# 5) 资源基线 + 现状证据（只采不执法；P.2.10 + 通信韧性评估 §6.1）：demo 窗口内
./target/release/iceoryx2_rt_baseline --duration 10m --hz 50   # synthetic transport latency
for u in xlerobot-s0-left-arm xlerobot-s1-left xlerobot-gateway-left \
         xlerobot-s0-right-arm xlerobot-s1-right xlerobot-gateway-right; do
  systemctl show -p MainPID $u.service; done                    # (comm,pid) join 映射

# 5b) 评估 §6.1 记录项（不改生产代码，只留证据）：
#     USB 拓扑 / 控制器归属（"不同 by-id ≠ 不同 root hub"）——从**权威
#     by-id 串号**解析，任何一步失败即中止（不吞错误）：
set -e
for ser in 5A7C121427 5A7C122340; do
  link=/dev/serial/by-id/usb-1a86_USB_Single_Serial_${ser}-if00
  dev=$(readlink -f "$link")
  syspath=$(udevadm info -q path -n "$dev")
  echo "== serial=$ser dev=$dev"
  echo "   syspath=$syspath"          # usbN 段 = root hub / 控制器归属
done
lsusb -t
#     控制器级 IRQ：cdc_acm 是 USB 类驱动，无独立 IRQ 行——有意义的记录是
#     xHCI 控制器的 IRQ 分布（两串口若同 xHCI 即同 IRQ 故障域）。
grep -iE 'xhci' /proc/interrupts
set +e
#     每 S0 的调度落点（左右各一次，保存对照）
for side in left right; do
  systemctl show xlerobot-s0-$side-arm.service \
    -p MainPID -p AllowedCPUs -p CPUAffinity -p CPUWeight \
    -p MemoryCurrent -p MemoryMin -p MemoryMax -p OOMScoreAdjust
  ps -L -p "$(systemctl show -p MainPID --value xlerobot-s0-$side-arm.service)" \
    -o pid,tid,psr,cls,rtprio,pri,ni,comm
done

# 5c) CPU/RSS 采样归档（Spec P.2.10：warmup 丢首帧后 ≥ 30 帧/进程）。
#     数据源事实：health 帧只由**两个 S0** 的 monitor 线程产生（robot-health
#     挂在 s0_daemon；S1/gateway 自身不发），日志正文是 "system health"，
#     interested 字段形如 `<pid>:<comm>@<cpu>%[<rss>M]`——每个 S0 的 monitor
#     按 comm 全局匹配，六个进程的 (pid,comm) 都出现在任一 S0 的 journal 里。
#     **权威采样源固定为左臂 S0 单条 journal**：两条合并时每个 monitor 各有
#     独立的 CPU 差分首帧，"减一帧"会计数膨胀（round 3 修正）。
#     [monitor] interval_secs = 1 → 尾窗 ≥ 40 s（丢首帧后仍 ≥ 30 帧）。
: "${DEMO_T0:?DEMO_T0 must be set before demo (§9 step 1)}"  # 守门：步 1 已记录，此处**不再赋值**
sleep 40
# F4（真机实测）：旧 journalctl 不认带时区的 ISO —— 用 date -d 转本地格式
T0_J=$(date -d "$DEMO_T0" "+%Y-%m-%d %H:%M:%S")
journalctl -b -u xlerobot-s0-left-arm.service \
  --since "$T0_J" --no-pager -o cat | grep -F "system health" \
  > /tmp/demo_health.log
fail=0
for u in xlerobot-s0-left-arm xlerobot-s1-left xlerobot-gateway-left \
         xlerobot-s0-right-arm xlerobot-s1-right xlerobot-gateway-right; do
  pid=$(systemctl show -p MainPID --value $u.service)
  [ -n "$pid" ] && [ "$pid" != 0 ] || { echo "FAIL: $u no MainPID"; fail=1; continue; }
  # 按**当前 MainPID** 逐进程计帧。F5（真机实测）：生产 unit 用
  # ROBOT_LOG_FORMAT=json，interested 是 JSON 字段 "interested":"<pid>:…"
  # ——不是控制台的 interested=<pid>:；前界 = 引号或 ','（防 12340 误配 1234）
  frames=$(grep -cE "\"interested\":\"([^\"]*,)?${pid}:" /tmp/demo_health.log)
  usable=$((frames > 0 ? frames - 1 : 0))   # 丢首帧（单一 monitor 源 → 恰一帧 CPU 差分无效）
  echo "$u pid=$pid frames=$frames usable=$usable"
  [ "$usable" -ge 30 ] || { echo "FAIL: $u usable=$usable < 30 —— 延长尾窗重跑本节"; fail=1; }
done
test "$fail" -eq 0   # ← 本节退出码（非零 = 取证不合格；归档前必须重跑，round 3 修正）
```

- 判据数值（AC-E12 ③，bin 内双层强制）：|target−anchor| ≤ 0.087 rad、
  斜率 ≤ 0.14 rad/s；缺省探针 = `*wrist_roll*` ±0.05 rad、出→回各 0.5 s。
- **应急协议**（§7 同文）：Enabled 期间 **S0 / S1 / gateway /
  `supervised_demo` 任一死亡或挂起** → 立即人工切断总电源并中止本轮。
- demo 结束宣称唯一口径 = **"受监督功能 demo 跑通"**。


---

## 10. iceoryx2 运行用户契约与 root 污染处置（F8，PX-60）

**契约（Reverse Sync 拍板 2026-09-01）**：

- 同一部署内**所有 iceoryx2 进程用同一个非 root 用户**（本机 = `loosqk`；
  七件 unit 的 `User=` 已如此）。
- **禁止 sudo/root 运行任何 iceoryx2 程序**——含 `iceoryx2_rt_baseline`
  等测量工具。RT 权限走 unit 的 `LimitRTPRIO`（服务）或给用户配
  rtprio rlimit（手跑工具），不走 sudo。
- 程序级 root fail-fast（启动时检出 euid=0 即拒）为后续独立小件，
  未实现前本节流程是唯一防线。

**污染症状**：非 root 进程结点创建裸报 `InternalError`
（`IOX2_LOG_LEVEL=trace` 也无路径细节）；s0 表现为
`s0_ipc_thread panicked ... InternalError` 连崩（guard 连败预算不受
影响——崩点在 exec 之后，成功 guard 已把计数写回 0）。

**处置步骤**（root 属主残留清理；实证 2026-09-01 15:36–15:40 事故）：

```bash
# 0) 预检：工具与权限缺失立即退出（fail-closed 的前提，错误不吞 stderr）
command -v fuser >/dev/null || { echo "ABORT: fuser 不存在（先装 psmisc）"; exit 1; }
sudo -n true || { echo "ABORT: sudo 不可用（需免密或先 sudo -v）"; exit 1; }

# 1) 停栈（六件；handoff 是 oneshot 无常驻）
sudo systemctl stop xlerobot-gateway-{left,right} xlerobot-s1-{left,right} \
  xlerobot-s0-{left,right}-arm

# 2) 三态存活判定——只有"已加载且 ActiveState ∈ {inactive, failed}"才算
#    通过；unknown / activating / deactivating / 查询失败一律退出。
#    注意：三个 daemon 的进程名都不含 "iceoryx2"，`pgrep iceoryx2` 证明
#    不了无人持有共享资源，必须按已知持有者逐一检查。
for u in xlerobot-s0-left-arm xlerobot-s0-right-arm xlerobot-s1-left \
         xlerobot-s1-right xlerobot-gateway-left xlerobot-gateway-right; do
  load="$(systemctl show -p LoadState --value "$u.service")" \
    || { echo "ABORT: $u LoadState 查询失败"; exit 1; }
  [ "$load" = "loaded" ] || { echo "ABORT: $u 未加载（LoadState=$load）"; exit 1; }
  state="$(systemctl show -p ActiveState --value "$u.service")" \
    || { echo "ABORT: $u ActiveState 查询失败"; exit 1; }
  case "$state" in
    inactive|failed) ;;
    *) echo "ABORT: $u ActiveState=$state（非 inactive/failed）"; exit 1 ;;
  esac
done
pgrep -a -f 's0_daemon|s1_daemon|robot_rpc_gateway|iceoryx2_rt_baseline|supervised_demo'
case $? in
  0) echo "ABORT: 疑似 iceoryx2 持有者进程仍存活（见上方列表）"; exit 1 ;;
  1) ;;   # 无匹配 = 通过
  *) echo "ABORT: pgrep 自身出错（退出码 $?）"; exit 1 ;;
esac
# shm 句柄三分支（不吞 stderr；命令放 if 条件里兼容 set -e）：
#   rc=0            → 有持有者，退出；
#   rc=1 且无输出    → 唯一放行分支：确认无持有者；
#   其余一切（rc=2/126/137…、或 rc=1 带输出）→ 检查失败，退出。
for f in /dev/shm/iox2_*; do
  [ -e "$f" ] || continue
  if out="$(sudo -n fuser -v "$f" 2>&1)"; then
    echo "ABORT: $f 仍被进程持有："; echo "$out"; exit 1
  else
    rc=$?
    if [ "$rc" -eq 1 ] && [ -z "$out" ]; then
      :   # 确认无持有者，继续
    else
      echo "ABORT: fuser 检查 $f 失败（rc=$rc）："; echo "$out"; exit 1
    fi
  fi
done

# 3) 确认无持有者后，单独执行清理
sudo rm -rf /tmp/iceoryx2 /dev/shm/iox2_*

# 4) 复位可能触发的 start-limit 并重启
sudo systemctl reset-failed 'xlerobot-*'
sudo systemctl start xlerobot-gateway-left xlerobot-gateway-right

# 5) 核验：非 root 结点创建恢复（任一 loosqk 身份 iceoryx2 程序可启）
```

> 污染点两处：`/tmp/iceoryx2/`（目录 750 root——`ls` 会报 Permission
> denied，**不要**用 `2>/dev/null` 吞掉它，会误判成"目录不存在"）与
> `/dev/shm/iox2_*_global_mgmt`（700 root）。
