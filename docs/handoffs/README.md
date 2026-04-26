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

## 列表

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
