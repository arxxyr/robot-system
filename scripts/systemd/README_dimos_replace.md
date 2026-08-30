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

## 6. 旧栈处置（demo 期间：**stop-only**）

- 允许：为释放硬件而 `systemctl stop`（handoff 的 `Conflicts=` 也会自动做
  这件事）。
- **禁止**：`disable` / 删除 unit / 移除 LCM 等运行时。
- 正式退役、`disable`、删除、LCM 随仓退出——**全部等 post-E 生产安全候选
  闭环**（决策单 D-BI3 r3）。

旧栈 unit：`xlerobot-coordinator.service` / `xlerobot-web.service` /
`xlerobot-s0-bus1.service` / `xlerobot-s0-bus2.service`。

---

## 7. 应急协议（Enabled 期间，权威文本见 Spec P.2.10）

**Enabled 期间任一 S0 进程死亡 / 重启 → 立即人工切断总电源并中止本轮。**

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
