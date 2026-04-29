# Handoffs（session 续接快照）

`docs/handoffs/<YYYY-MM-DD>.md` 每一份 = 一次 session 结束时的进度 + 决定快照，供下一次 session（尤其是异地续开、换机器、隔几天回来）**5–10 分钟读完就能 resume**。

## 这是什么 / 不是什么

| 这是 | 这不是 |
|---|---|
| 本次 session 做了什么 + 下一步做什么 | 设计 spec（去 `00_design.md`）|
| 关键决定的**摘要 + 指针**（详情去 CHANGELOG） | 决定的权威记录（去 `CHANGELOG.md`）|
| 当前状态（在 v0.x 哪一步 / 什么卡住） | 指标 baseline / 测试状态（去 `10_verification.md`）|
| 异地 resume 的 bootstrap 指南 | 长期维护的 onboarding（去 `02_start_here.md`）|

## 改动规则

- **非 spec，自由更新 / 追加**，不走 `CHANGELOG.md` 登记。
- 每次 session 结束追加一份 `<YYYY-MM-DD>.md`。同日多次 session 可加后缀 `<YYYY-MM-DD>_<n>.md`。
- 过期的 handoff **保留不删**（作为演进历史）；但不是 onboarding 的阅读材料，新人读 `02_start_here.md` 即可。

## 怎么用（新 session 开场）

```
# 拉代码（服务器或别的机器）
cd ~/repo/rust/robot-system && git pull

# 按时间降序读最新 1-2 份 handoff
ls -t docs/handoffs/*.md | head -2 | xargs -I {} less {}

# 然后按 handoff 末尾 "开工前必读" 补齐语境
```

## Milestone anchor（**不**是 daily snapshot）

跟下面 daily 列表区别开，专门挂 milestone 入口的 handoff —— 这些是 onboarding
/ 跨阶段交接时的**当前**权威读物，不是历史快照。

| Milestone | 用途 | 文件 |
|---|---|---|
| **v0.3 entry** | v0.2 → v0.3 边界 + 第一批工作项硬契约（ClearEStop control-plane slice / freeze hook + e2e / 禁止项）；reviewer 先用本 handoff 做文档审核再放第一批实现 | [v0.3_entry.md](v0.3_entry.md) |
| **v0.3 A1-4 clear_estop smoke** | A1 ClearEStop control plane 端到端 smoke 实证封存 + 可复跑入口（`scripts/run_clear_estop_smoke.sh`）；A1-1 → A1-3 (3/3-b) 完整 43 个 commit chain；§6.7 #3 仍降级宣称（A1-5 闭环时升） | [v0.3_a1_4_clear_estop_smoke.md](v0.3_a1_4_clear_estop_smoke.md) |
| **v0.3 B1 outline** | Phase B 第一条工作项 outline：`s1_daemon` 替代 `mock_s1` 最小闭环（hold + heartbeat + 启动锚定 fail-fast）；reviewer 7 决策点已拍板（binary 名 / config 入口 / on_timeout / mock_s1 保留 / 等）；commit 拆分 3 条 | [v0.3_b1_outline.md](v0.3_b1_outline.md) |
| **v0.3 B2 outline** | Phase B 第二条工作项 outline：policy/action interface + robot profile matrix 设计；定义 `ActionRequest` / `ActionBackend` trait / `CoordinatorState`（IDLE/EXECUTING/DEGRADED）；reviewer 4 阻断 + 11 决策已拍板（2026-04-28，撤 tokio / 撤 `ActionKind::Stop` / DEGRADED 重新定义为 S1 内部状态 / 加双路 smoke 验收门槛）；commit 拆分 3 条 | [v0.3_b2_outline.md](v0.3_b2_outline.md) |
| **v0.3 B2 summary** | B2 (1/3)+(2/3)+(3/3) 全部 land 后的封存锚点：`Coordinator` 5-state 机 + `BackendRegistry` + `MockDwellBackend` + `run_action_tick_loop` state-aware dispatch + `s1_daemon` wiring 真接入；reviewer 多轮拍板 11 决策 + 4 outline 阻断 + 2+1 实现期 fix（state-priority / NoEnabledBackends / 注释 #3）全闭环；双路 crash_restart_smoke 5/5+5/5 PASS；151 test 全过 | [v0.3_b2_summary.md](v0.3_b2_summary.md) |
| **v0.3 B3 outline** | Phase B 第三条工作项 outline：第一个真 action backend；默认推进 `HoldPose`（复用 B1 hold builder + 不扩 `ActionKind` enum + 空 payload + 2 commit 节奏）；11 待拍板项已列；reviewer 拍板前**不**进实现 | [v0.3_b3_outline.md](v0.3_b3_outline.md) |

## 列表（daily session snapshots）

| 日期 | 主题 | 文件 |
|---|---|---|
| 2026-04-24 | Target 文档三分重构 + RT 线程拓扑方案 B + policy_host RT 内核判断 + iceoryx2 RT baseline 提前到 Step 6 | [2026-04-24.md](2026-04-24.md) |
| 2026-04-24（下午）| Orin Nano Super bringup：workspace 全绿 + vendor flatc aarch64 + cyclictest 50Hz max 94-127µs | [2026-04-24_2.md](2026-04-24_2.md) |
| 2026-04-24（晚上）| Step 6 Part 1 落地：6 crate + 1 bin + 173 tests + seqlock/arc-swap 同步原语 + 5 主态 state machine | [2026-04-24_3.md](2026-04-24_3.md) |
| 2026-04-24（深夜）| Step 6 Part 2 基础设施（task 10-12/14）：build_helpers + estop_latch + watchdog；192 tests；剩 11 task 留下次 session | [2026-04-24_4.md](2026-04-24_4.md) |
| 2026-04-24（凌晨收尾）| Step 6 Part 2 完整落地（task 13-23）：S0 四线程 + mock_s1 + robotctl 闭环跑通；Orin baseline FAIL 量化（系统调优阻塞） | [2026-04-24_5.md](2026-04-24_5.md) |
| 2026-04-24（深夜）| Step 7 task 24-28 全落地：CI smoke + ConfigIdentity + rt_guard soak + feetech crate（83 unit + 3 集成 smoke + criterion bench）+ inventory matrix + flamegraph baseline + by-id hot path -54% + Orin USB-RS485 真硬件 ping smoke 通过 | [2026-04-24_6.md](2026-04-24_6.md) |
| 2026-04-25 | Step 7 task 29 完整落地：feetech calibration JSON loader + silent clamp + 防飞 enable 序列 + `hold_current_position` helper + `AdapterBuildContext` 扩 `calibration_paths`；**Orin 真机闭环真转左臂夹爪 ±5°，hold_current 语义真机数据验证通过** | [2026-04-25_1.md](2026-04-25_1.md) |
| 2026-04-26 | **task #15 Stage 1 真机验证全过**：bus2 右臂 6-motor SerialRT 端到端在 Orin 真硬件跑通（§2 ping → §3 smoke → §4 ballet → §5 EStop → §6 重启 latch=true → §7 删 latch 恢复→再 EStop）；阻断 1 fsync 落盘 200 ms 内、broadcast Torque_Enable=0 真断扭、latch 重启不失忆全部成立。**iceoryx2 0.8.1 multi-sub race** 暴露（不阻断物理验证；待 follow-up）。**sudo↔user mix → owner 残留** 暴露 + 修补。同 session 之前还落地了 task-30 v9 M7 全栈（state_machine ↔ adapter 接通、SerialRT 路径、ipc_tick errata 三步式、per-event guard view、ClearEStop confirmed 守门、EStop 触发即 fsync 落盘） + `robotctl estop` 子命令 + bus2 right_arm 配置 + Orin runbook。 | [2026-04-26.md](2026-04-26.md) |
| 2026-04-26（续）| Stage 2 软件就绪 + Heartbeat QoS bug 撤销 + `00_design.md` §8 tracelog 格式回写。`TopicClass::Heartbeat` (max=8 + safe_overflow) 替换 hb 站点蹭用的 `S1CmdVel`（max=1）；2 sub 同 service 锁测 + S1CmdVel max=1 反向锁测；mock_s1 加 `--frozen-joints` mask 模式 + `xlerobot_bus2_full.toml` (9-motor)。**待 Orin 真机端到端 Stage 2 验证**。 | [2026-04-26_2.md](2026-04-26_2.md) |
| 2026-04-26（再续）| **task #15 Stage 2 真机闭环全过 + §5.1 known-issue 真机撤销**：Orin 9-motor SerialRT 跑通 §2-§7（含 Operating_Mode + EEPROM unlock 五步序列、双 watch 各 45 sample QoS 修复回归、broadcast Torque_Enable=0 覆盖 base 9 motor、base 不动锁测 Δ=0/0/0、latch fsync ~250ms 落盘、latch=1 重启不进 Enabled）。下一步候选：Step 9 crash/restart 测试套 / systemd 集成 / task #16 eprintln→tracing 全量迁移。 | [2026-04-26_3.md](2026-04-26_3.md) |
| 2026-04-26（四续）| **v0.2 收尾全部闭环 + reviewer 三轮否决全过**：Step 9 crash_restart_smoke 5/5 PASS（结构化字段 + JSON + NO_COLOR）+ systemd Type=notify 双 bus（PrivateTmp 删 + ProtectSystem=strict→full + watchdog 真重启）+ task #16 eprintln→tracing 迁移 ~58 sites（lib + 生产 daemon；CLI 故意保留）。reviewer round 2/3 三阻断：iceoryx2 跨进程 + tracing stdout/ANSI grep 失效 + systemd_notify env 竞争，全部修完真机回归。下一步：v0.3。 | [2026-04-26_4.md](2026-04-26_4.md) |
| 2026-04-26（五续）| **v0.2 收尾 reviewer round 4 阻断闭环 + 宣称降级**：commit 3a1cca2 文档登记后 reviewer 复核否决"§6.7 全锁住"宣称。修两条代码语义债：阻断 1 watchdog 真接 4 worker `tick_count` observer（liveness-gated，任一停滞不喂 systemd），happy path 真机 0 stalled warning + pid 不变；阻断 2 latch 缺失 fail-closed，新增 `MissingLatchPolicy::Strict`（生产默认拒启动）+ `--allow-empty-latch` opt-in（首次部署），真机两路验证。**宣称降级**：§6.7 #4 真锁住；#2/#3 代码层面到 reviewer 边界（cap-deadlock 在 daemon 内 freeze hook + ClearEStop RPC 留 v0.3）。 | [2026-04-26_5.md](2026-04-26_5.md) |
| 2026-04-26（六续）| **v0.3 A2 freeze hook 全栈落地 + e2e 真机回归**：A2-1 (5 worker tick loop hook + `test-freeze-hook` feature 守门 + 单测 8 个) + A2-2 (test-only iceoryx2 topic `/<robot_id>/test/freeze` + control thread 路由 + monotonic clock 算 absolute ns + 单测 4 个) + A2-3 (user-service 兼容 unit 模板 + `freeze_systemd_smoke` binary + 本机 5/5 PASS) + A2-4 (Orin AGX 1 round PASS)。reviewer 三轮阻断：round 1 (8 条硬边界 lock) / round 2 (preflight 收紧 + 重启验证 `is-active` 二次确认 + 4 条 journal 硬断言拆分) / round 3 (Orin journal query 跨发行版兼容 `_SYSTEMD_USER_UNIT` matcher)，全部修完。**§6.7 #2 真宣称升级 + 模板 `state=Safe` 字面修复留 A2-5**。 | [2026-04-26_6.md](2026-04-26_6.md) |
