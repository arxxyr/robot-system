# CodeMap · v0.5 候选 D — `EStop` 触发 RPC（Pre-Research，不开实现）

> 范围：实现 design §5.3 已冻结但未 land 的 `EStop` 触发 RPC（**不**是 ClearEStop，
> 后者 A1 已 land）。本文是 RIPER 第一阶段 Pre-Research：摸清既有 EStop 通路 +
> design/entry 两份文档的路由张力 + gateway/auth 现状，给 Spec round 1 的拍板
> 清单。**0 代码改动**。
>
> 入口：[`docs/handoffs/v0.5_entry.md`](../../docs/handoffs/v0.5_entry.md) §2.4
> + [`docs/handoffs/v0.5_set_enabled_summary.md`](../../docs/handoffs/v0.5_set_enabled_summary.md) §8 接班。

## CodeMap amend 记录（round 1；reviewer 复核后）

reviewer 2 阻断 + 1 非阻断，已回修：

| # | 阻断 | 修订 |
|---|---|---|
| B1 | **`EStopEvent.latched` 语义漏出 Q 表**：`event.fbs:9` 字面 = "S0 已经把 latch 持久化到磁盘"，gateway publish 路由下 publish 时刻**无法知道** S0 是否 fsync → 不能沿 robotctl 现状填 `true`（wire 上说尚未发生的事实）。robotctl `latched=true`（robotctl.rs:350）是**既有语义债**（producer 在 S0 消费前就填 true，同样不可知；其 `source="robotctl"` 也不在 §3.5 三枚举内），不能当新 RPC 语义依据 | §2 加 latched 语义债 callout；**新增 Q8**（三选项：publish 填 `false`=“触发请求未确认” / 重解释 latched=“请求 latch true”并登记语义债 / 走 S0 回执路径让 true 只在确认后出现）|
| B2 | **route (c) / GetState "观测 latch" 说过头**：`RobotState` 只有 mode/control_link/output_mode/safety_state/fault_code，**无 `estop_latched` / fsync 状态字段**。GetState 能证 Safe / StopCommanded / TorqueOffConfirmed 等**运行态**，**不证 latch 文件已 fsync** | §5 (a)/(c) + Q1/Q2/Q7 表述收窄：(c) 改名 "publish + **安全态观测**"（非 latch 确认、非强 Ack 等价 RR）；强 Ack = latch fsync 已完成**只能**走 S0 回执路径或新增可观测字段/机制 |
| 非阻断 | QoS anchor 写短易误读 | 改全路径 `crates/robot-transport/src/iceoryx2/qos.rs:139-146` |

## 1. design frozen 形态（权威字面）

- **§5.3 proto（frozen，未 land）**（`docs/00_design.md:909` + `:939`）：

  ```proto
  message EStopCommand { CommandMeta meta = 1; string estop_reason = 2; }
  rpc EStop (EStopCommand) returns (Ack);
  ```

  **注意**：与 `ClearEStopCommand` 不同，**无 `confirmation` 字段**——EStop 是
  安全方向（上锁），不需要人工确认串；自洽，不是遗漏。
- **§6.6 EStop 一致性（路由字面）**（`docs/00_design.md:1121-1140`）：

  ```
  RPC EStop → gateway → iceoryx2 /events/estop → S0 latch SAFE + 写磁盘
  ```

  design **字面**把 RPC EStop 列为 estop topic 的一个 **publisher 源**（与
  button / s1_detect 并列）——即 gateway **publish topic**，非 RR。⚠ 与 entry
  表述存在张力，详 §5。
- **§5.4 role**：`estop` capability = **operator+**（`configs/rpc_roles.toml:54-59`
  operator 含 `estop`；maintainer/admin 同）。**触发=operator+ / 清除=maintainer+**
  的分权模型（ClearEStop 的 maintainer-only 由 `auth.rs:132-150`
  `check_clear_estop_invariant` 硬锁；**EStop 触发无此类反向锁**——operator 可触发
  是设计意图）。
- **`Capability::EStop` 已存在**（`crates/robot-rpc/src/auth.rs:72` enum +
  `:87` `as_str = "estop"`）——A1 时代占位，gateway 端**尚无**任何 RPC 消费它。

## 2. 既有 EStop 数据通路（S0 消费链已完整，候选 D 不需要新建）

```
[publisher] ──/<robot_id>/events/estop（EStopEvent fbs）──▶ s0 ipc_thread
  poll_estop（ipc_thread.rs:1153-1167，latest() + parse 计数）
    → events.push(Event::EStop)（:676-678）
    → ipc_tick drain_emergency（ipc_tick.rs:128-202）：
        guard 副作用 estop_latched=true（:383）
        + executor.safety_stop()（hard_rt_ctrl.rs:142-185：emergency_action CAS
          + safety_latched=true + OutputSafetyState::StopCommanded；rt_thread
          下 tick broadcast Torque_Enable=0）
    → state machine (_, EStop) → Safe（state_machine.rs:554-555，压倒一切含
      FAULT/STOPPING；TransitionKind::Emergency + OutputPolicy::StopTorque）
    → 主循环 §6.5 latch fsync hook：save_estop_latch(path, true)
      （estop_latch.rs:188-192，primary-first 双写；§6.7 #4）
```

- **wire schema 复用**：`EStopEvent`（`event.fbs:9-15`：reason / stamp_ns /
  source / latched）schema id **`0x0000_0300`**（`schema_id.rs:53-54`）；markers
  + `build_estop_event(reason, stamp_ns, source, latched)`
  （`build_helpers.rs:207-228`）全部已有。**候选 D 0 个新 fbs schema**。
  ⚠ **entry 勘误**：`v0.5_entry.md:143` 写 "schema_id 0x0405" 是**笔误**，实际
  `0x0000_0300`（0x0405 不存在于 SCHEMA_REGISTRY）。
- ⚠ **`latched` 字段语义债（B1，Q8 必拍）**：`event.fbs:9` 字面 = "`latched`
  告诉订阅者 **S0 已经把 latch 持久化到磁盘**（§6.6）"——这是 **S0 侧事后状态**，
  任何 producer 在 publish 时刻都**不可知**。既有 robotctl 直连填 `true`
  （robotctl.rs:350）= **既有语义债**（publish 早于 S0 消费，更早于 fsync；其
  `source="robotctl"` 也不在 §3.5 "button/rpc/s1_detect" 三枚举内）——只能作历史
  现状，**不能**当新 RPC 语义依据。gateway publish 路由下 `latched` 填值三选项
  见 Q8。
- **topic QoS**（`crates/robot-transport/src/iceoryx2/qos.rs:139-146`，
  `TopicClass::SafetyEvent`）：history 32 / sub buffer 64 /
  **max_publishers = 4** / max_subscribers 8 / `safe_overflow = false`（不丢，
  满即 fail）。注释明文预留 "s0 + s1 + external estop + manual trigger"。
- **publisher 占用实测**（候选 D 槽位预算）：生产常驻 = **0**（S0/S1 只订阅，
  `robot-s0` 内的 `publisher::<EStopEvent>` 全在 `#[cfg(test)]` harness：
  ipc_thread.rs:1370/1848/2408）；瞬时 = `robotctl estop` 每次调用 attach→publish
  →exit（robotctl.rs:323-382，source="robotctl" / source_id=99 / latched=true）。
  → gateway 若常驻 1 个 publisher，预算 4 槽宽裕。

## 3. 既有 producer / 消费现状盘点

| 角色 | 现状 | 候选 D 关系 |
|---|---|---|
| `robotctl estop`（CLI 直连） | ✅ 已 land（iceoryx2 直接 publish，不经 gateway） | **保留**——局域网内不依赖 gateway 存活的紧急路径；RPC 是**远程/审计**路径，两者并存（Q6：robotctl 是否加 estop-rpc 子命令推 Spec）|
| 硬件按钮 / s1_detect | design §6.6 列出的源；当前无实现 | 不在候选 D |
| gateway | **无** EStop 入口；proto 9 method 无 EStop（`robot_control.proto:125-144`；header `:22-23` 列为 deferred）| 候选 D 主体 |
| e2e 既有覆盖 | `clear_estop_e2e`（latch 预置→Clear）；`crash_restart_smoke`（EStop→SIGKILL→重启 latch 不失忆）；`robotctl_smoke`（estop phase）；`set_enabled_e2e`（latched 预置 enable 拒）| EStop **publish→latch fsync→Safe** 链已被多 harness 锁；候选 D e2e 聚焦 **RPC→** 段 |

## 4. gateway 基建现状（实现面预估）

- **可直接复用**（候选 C 12 步骨架）：`validate_request_id` / `RoleTable` authz
  （`Capability::EStop` 现成）/ `IdempotencyCache`（`Method` enum append）/
  `rpc_status_to_tonic_code`（若走 RR）/ `Ack` 回执。
- **gateway 当前无 pub/sub publisher 基建**：robot-rpc 只有 RR clients
  （rr_io_thread ×6）+ 只读 subscriber（state_pool / events_subscribe）。若走
  路由 (a)（publish topic），publisher 模式是 gateway **新 pattern**——但
  `robotctl.rs:323-382` 有完全同构的 publisher 先例可抄（Node + open_pub +
  loan/publish），工程量小；真正要拍的是**语义**不是工程量（§5）。
- 若走路由 (b)（RR control plane）：S0 加**第 3 个** rr_server + ipc drain +
  action（沿 set_enabled 范式 copy，候选 C summary §8 已预判）；gateway 加第 7
  条 rr_io_thread。

## 5. ⚠ 核心路由张力（Spec round 1 必拍，Q1）

两份已 ACK 文档对"RPC 怎么到 S0"说法**不一致**：

| | (a) gateway publish estop topic | (b) RR control plane（沿 A1/候选 C） |
|---|---|---|
| 文档背书 | **design §6.6 字面**（"RPC EStop → gateway → iceoryx2 /events/estop"）| **entry §2.4 字面**（"沿 A1 ClearEStop control action 同款 control plane 而非 RPC 旁路"）|
| S0 改动 | **0**（poll_estop→emergency drain→latch fsync 全既有，§2）| 第 3 个 rr_server + drain + action（~candidate C (3/5) 量级）|
| 触发延迟 | 1 跳（publish 即生效，与按钮/robotctl 同路）| 2 跳 + RR 排队/deadline 机制 |
| Ack 语义 | **弱**：Ack = "已 publish"，**不**证 S0 已消费/latch 已 fsync（pub/sub 无回执）。客户端经 `GetState` 可观测的只有**运行态**（mode=Safe / safety_state=StopCommanded\|TorqueOffConfirmed）——`RobotState` **无 `estop_latched` 字段，latch fsync 远程不可观测**（B2）| **强**：S0 侧 latch fsync + safety_stop 提交后回执（同 ClearEStop `Cleared{at_ns}`）——**唯一**能让 "latched 已持久化" 上 Ack 的路径 |
| 失败面 | publish 失败可知；S0 死无回执（但 **§6.7 #1**：S0 不在时硬件 fail-safe 兜底，EStop RPC 本就不是该场景的救命索）| RR timeout 可区分 "S0 不在"；但 emergency 走排队语义反直觉 |
| 单飞/幂等 | 不需要（latch 单调 true，重复 publish 无害）| 沿用单飞会让第二个 EStop 被拒——**emergency 被拒绝是错的** |

**对称性论证不成立的点**（CodeMap 立场，供拍板）：ClearEStop 走 RR 是因为它是
**危险方向**（解锁），需要 TorqueOffConfirmed gate + confirmation + 强 Ack；
EStop 是**安全方向**（上锁），无 gate、无确认、任何时刻执行都对——机制不必对称。
design §6.6 的 (a) 正是这种不对称的体现；entry 的 "对称 control plane" 写于
候选 D 评估期（粗粒度），design 是冻结权威。**倾向 (a)**，代价 = Ack 弱语义须
在 Spec 明文宣称（Ack=published；远程客户端经 GetState 只能观测**运行态**进入
Safe/StopCommanded，**latch fsync 不可远程观测**——`RobotState` 无 estop_latched
字段，B2；latch 文件断言只在 e2e harness 本地可做）。
备选 (c) = **publish + 安全态观测**（gateway 侧等自己的 state cache 观测到
mode=Safe / safety_state=StopCommanded|TorqueOffConfirmed 再回 Ack）——比 (a)
多证 "S0 已消费并进入安全态"，**但不是 latch 确认、不等价 (b) 的强 Ack**（fsync
是否完成仍不可知）。**强 Ack（= latch fsync 已完成）只能走 (b) S0 回执路径，或
新增可观测字段/机制**（如 RobotState 加 estop_latched——wire 扩字段，另立拍板）。

## 6. §6.7 关联

| # | 影响 |
|---|---|
| #1 S0 存活=安全充分条件 | 不变；**RPC EStop 不是 S0 死亡场景的救命索**（那是硬件 fail-safe），Spec 须明文 |
| #3 EStop 不可清为失忆态 | **强化**：新增一个显式、带审计（CommandMeta source/audit_reason → wire EStopEvent.reason/source）的 latch 源；清除仍唯一走 ClearEStop（maintainer+），分权模型 §1 |
| #4 重启不失忆 | 不变（latch fsync primary-first 既有；crash_restart_smoke 已锁）|
| #5 幂等写 RPC | EStop 天然幂等（latch 单调 true + 重复 publish 无副作用差异）；IdempotencyCache 是否仍挂（统一骨架 vs 特判）→ Q4 |

## 7. open questions（Spec round 1 拍板清单）

| Q | 问题 | CodeMap 倾向 |
|---|---|---|
| Q1 | **路由**：(a) gateway publish topic（design §6.6 字面）vs (b) RR control plane（entry 字面）vs (c) publish + **安全态观测**（gateway 等自己 state cache 见 mode=Safe / safety_state 安全值再回 Ack；**非 latch 确认**，B2） | (a)，详 §5；entry 表述按 CHANGELOG 勘误 |
| Q2 | **Ack 语义**：`cleared_at_ns` 承载什么时刻 + 弱/强宣称。可选：(a) published_at_ns（弱）/ (c) observed_safe_at_ns（中：S0 已消费进安全态，仍不证 fsync）/ (b) latched_at_ns（强，**仅** S0 回执路径可承诺）。`RobotState` 无 estop_latched 字段 → latch fsync **远程不可观测**，弱/中宣称必须明文 | (a) 下 = published_at_ns + 明文 "不证 S0 已消费 / latch 已 fsync" |
| Q3 | **dry_run 语义**：EStop 无 precondition gate，dry-run 评估什么？ | 倾向**拒**（InvalidArgument，"EStop 不支持 dry_run"）——比候选 A "假 dry-run" 洞和无意义的 would-publish 都诚实；推 Spec 拍 |
| Q4 | **idempotency cache**：天然幂等下仍挂 `Method::EStop` 统一骨架，还是特判跳过？ | 挂（统一 12 步骨架 + replay 返同 Ack，代价≈0；§6.7 #5 字面满足）|
| Q5 | **wire 映射**：`EStopCommand.estop_reason` + `meta.source` → `EStopEvent.reason/source`；`source` 固定 "rpc"（event.fbs:13 注释枚举）还是透传 meta.source？ | source="rpc" 固定（fbs 注释字面）+ reason 透传 estop_reason；audit_reason 进 gateway log 不进 wire |
| Q6 | robotctl 是否加 `estop-rpc` 子命令（经 gateway，区别既有直连 `estop`） | 推后（候选 D 不捆绑 CLI；直连路径已覆盖本地紧急）|
| Q7 | **e2e 拓扑**：沿 set_enabled_e2e（s0+gateway）。观测面分两层：远程 = GetState 运行态（mode=SAFE + safety_state）；**latch 文件 "1" 断言 = e2e harness 本地读文件**（远程客户端做不到，B2）。是否需 heartbeat 源先 Enabled 再 EStop 看降级？ | 两 case：Safe 态 EStop（最小）+ Enabled 态 EStop（heartbeat 源，证 Enabled→Safe 压倒）；viewer 拒 + operator 准（分权）；latch 断言走 harness 本地文件 |
| **Q8** | **⚠ `EStopEvent.latched` 填值（B1）**：fbs 字面 = "S0 已持久化到磁盘"，gateway publish 时刻不可知。三选项：(i) 填 **`false`** = "触发请求，未确认 latch"（语义诚实；消费面实测无破坏——生产订阅者**只有** s0_daemon，`poll_estop` 不读该字段只 parse 计数〔ipc_thread.rs:1153-1167〕；`.latched()` 读者全在测试侧且绑 robotctl 直连路径〔compat.rs:346 / build_helpers.rs:852 roundtrip + robotctl_smoke.rs:580〕，与 RPC 路径不交叉）/ (ii) 重解释 latched = "请求 latch true" 并**同步登记**这是对 event.fbs:9 注释的语义改写 + robotctl 既有 true 的债务追认（CHANGELOG）/ (iii) 走 (b)/S0 回执路径，`true` 只在 S0 确认后出现 | (i)，配 fbs 注释澄清 + robotctl 债务登记；与 Q1 路由联动拍 |

## 8. 实现量预估（路由 (a) 口径）

proto +1 method（9→10）+ gateway handler（publish 路径，无 RR）+ 常驻 estop
publisher + e2e；S0 **0 改**；fbs **0 新 schema**。估 3-4 feat commit（< entry
的 5-8，因路由 (a) 砍掉 S0 段）。路由 (b) 则回到 entry 的 5-8 估计。
