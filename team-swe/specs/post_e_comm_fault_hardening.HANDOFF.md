# 交接 · post-E 通信故障硬化

## 当前状态（2026-09-08）

C1–C5 已独立评审并提交，C5 基线 `85efefb`（未 push）。
[Spec](post_e_comm_fault_hardening.md) / [PLAN](post_e_comm_fault_hardening.PLAN.md)
的 S-44 / P.6.10 已获独立 **APPROVED**：两端各 57 项工具测试与真实 mock 回归
通过，Orin 两核 mock S0 的实际调度通过，左臂 H1/H3 原始记录已独立复核。
当前源码 `d6963ed017541d74b8733946c54d9243ece9b7691b8d5e72b1fa37966cf4b11a`
（308 输入）；证据为 [H.json](../../.verify/post_e_comm_fault_hardening/H.json) 与
[H_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)。
本文件只保留当前执行事实；已结束的逐轮评审流水账与重复提纲按用户要求删除。

C5 源码 `e6d2b30699379ee0e2222649efb4e79093978c612e0d6a67bc83dce9720d2598`
（308 输入）获 **APPROVED_SOFTWARE_SCOPE / SOFTWARE_ACCEPTED_HARDWARE_PENDING**。
两端各 1737 PASS / 11 ignored、67 软件场景、八套 smoke；Orin 四项真实 RT guard
全部通过。完整范围见 [10_verification](../../docs/10_verification.md)，复现见 PLAN P.6。

C5 当前证据为 [C5.json](../../.verify/post_e_comm_fault_hardening/C5.json) 与
[C5_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/C5_evidence.tar.gz)，包 SHA
`343dba15f58dcae362464b4a5829e6d855fadc262aaaae2b4214355e43a5c819`。
两端旧验收目录、重复产物与历史压缩包已清理；复跑按包内 manifest 重建二进制。
不恢复旧过程数据，不修改 Git 历史，活跃 Cargo 缓存与四个无 hook 正式产物保留。

## 当前工作与现场边界

- 主 agent：Spec / PLAN / 当前文档、独立 `sla_acceptance.rs`、本地 Cargo、整合提交。
- developer：只写 `comm_fault/sla.rs`，实现物理 S0 参数、普通 CPU 掩码继承、
  三进程逐 TID 真实调度检查；首次恢复/使能前先留完整报告，失败走既有独立收尾。
- reviewer：独立 Plan / 代码 / 证据评审；开发自测不替代验收。
- Orin owner：独占远端 Cargo 与执行器；隔离验收最终产物，核正式四 bin SHA 不变。

用户已明确确认“左臂已完成，支撑与现场断电手段就绪”，对应双臂支撑、清空
接触区、左臂电机断电重上电及物理 USB 拔插。左臂 H1/H3 已通过；右臂尚未收到
其物理步骤完成信号。用户已再次确认左臂电源循环/物理 USB 拔插完成，此后未开总线；
工具包评审提交并对齐后即可 D9 一次性授权 → 左臂 H2。
H1 只做 100 warmup + 1000 次完整 ping 特性记录，不设响应时延 PASS 门；H3 的
100 warmup + 1000 次全六 ID 断扭广播只证明主机调用 ≤10ms、一写零读及完整帧。
H2 在 S-44 通过代码与必要质量门后再执行：每臂四组各 100 warmup + 1000 样本，
反复恢复/使能/断扭，单臂至少约 44 分钟；用户已确认可以持续在场完成左臂 H2。
开始前须新物理边界
及 D9 canonical revoke → authorize；marker 缺失不代表授权，不自动反复授权。

Orin `loosqk@10.0.1.40`，原仓库两端基线一致；左右 by-id、D2 配置、标定、权限已
只读核验，六个常驻 S0/S1/gateway 服务保持 inactive/PID0，串口无占用。
CPU0–5 在线，4–5 isolated；普通 loosqk rtprio99、memlock 无限、NOFILE1024。
需要权限直接用已授权 sudo，但 sudo 本身不会配置 FIFO/绑核。工具须在 Tokio
runtime 前由 `taskset -c 0-3` 启动；串口线程独立 FIFO70、左4/右5。快照只证明
取证当时环境，不声称连续监控或 PREEMPT_RT 内核。

左臂 H1/H3 PASS，H2 与右臂未测，D15 **OPEN**，`deployable=false`；不自动启动六个常驻服务。
推进硬序：通信硬化现场验收 → D15 退役决策 → Rerun 桥 → F7 判据小件。
RX 重同步、串口 reopen、跨臂联锁不在本候选；不改冻结 `docs/00_design.md`。
每阶段独立评审及 fmt / Clippy 等必要门通过后 commit，不 push；同机 Cargo 串行。
