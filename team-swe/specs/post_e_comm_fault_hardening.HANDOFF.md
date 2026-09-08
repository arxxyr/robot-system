# 交接 · post-E 通信故障硬化

## 当前状态（2026-09-08）

C1–C5 已独立评审提交，C5 `85efefb`；S-44 工具 RT 修复与左 H1/H3 已批准并提交
`a161a7b`。左 H2 首次 **FAIL**，真实断扭及进程收尾完成；
S-45 / [PLAN P.6.11](post_e_comm_fault_hardening.PLAN.md) 实现与两端软件验收已独立
**APPROVED_SOFTWARE_SCOPE**，已提交 `965bcd7`，未 push。[Spec](post_e_comm_fault_hardening.md) 是有效语义。

- 左 H1：100 warmup + 1000 次六电机轮询完整 ping，零错误；时延只记录特性。
- 左 H3：100 warmup + 1000 次完整六 ID 断扭广播，一写零读，正式 host 调用最大
  0.491552ms；不据此保证 UART 发完或机械断扭。
- 左 H2：三进程实际 RT 门 PASS（工具/S0/GW 各 6/11/14 TID）；首组 35 次真实
  在途分类失配，第 36 次 epoch10072 缺同 epoch acquire，严格分析器拒绝，正式0。
  无 dropped 或通信 Fault，不能把采样缺陷写成已证实总线故障。
- `final_stop` 的真实 EStop 确认状态3，Disable 成功；五测试 PID 全退、左串口无占用，
  六服务 inactive/PID0，双 marker `1\n`、UID/GID1000、0600，正式四 SHA 未变。
- 右 H1–H3 未测，D15 **OPEN**，`deployable=false`，不自动启动常驻服务。

当前证据唯一入口 [H.json](../../.verify/post_e_comm_fault_hardening/H.json) 与
[H_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)。首次 H2 FAIL
与 S-45 软件证据均已纳入该唯一归档；当前活动目录为 `target/post_e_h2_rt/orin/`。
S-44/首次 H2 源码 `d6963ed0…4b11a`（308 输入），构建时 base=`85efefb`，不能反填
后续提交身份。C5 源码 `e6d2b306…2598`（308 输入），两端各1737 PASS/11 ignored、
67软件场景、八套smoke、Orin四RT guard通过；[C5.json](../../.verify/post_e_comm_fault_hardening/C5.json)
与唯一C5包保留。已结束的流水账/重复产物不恢复；活跃Cargo缓存与正式产物保留。

## 当前分工与下一步

主 agent 维护 Spec/PLAN/文档、独立 acceptance、本地 Cargo、整合提交；developer
负责 S-45 trace/hook/serial tick/SLA 工具；reviewer 独立审查；Orin owner 独占远端
Cargo 和现场执行。共享文件按 owner 写，同机 Cargo 串行。每阶段独立评审和 fmt /
Clippy 等必要门通过后 commit，不 push，不改冻结 `docs/00_design.md`。

S-45 在真实 serial 无 lease 边界落实 arm/disarm，tool 等同 seq 真实回执。idle
前置门各用固定1秒截止，普通准入受控，急停/故障/取消/退休不受阻；inflight 保留真实
事务及 U(1,5)ms 载荷，原 analyzer/样本数/P99 不放宽。真实 TracePage+probe 的
acquire→arm→release 已确定性 RED→GREEN。Orin 首轮软件门17项通过，完整67场景
为66 PASS/1 FAIL：工具混读旧快照与新 CommandApplied，将已接受请求误判拒绝；
另有81条已提交未消费事件证明 serial 实际接受。修订先 Acquire 读确认再读边界，
新增确定性交错与真实拒绝回归通过，源码 `c07a9886…514b1`/309 输入已独立批准；
本地8项门及完整67场景通过并独立 ACK。Orin 复验发现启动测试缓存绑定旧产物路径，
仅重编该 harness 后原8项启动测试通过；最终18门、四RT guard及67场景通过。
18门证据由 `orin/s45/recheck/run.json` 前8门与 `continuation/run.json` 后10门
组成，另留一次具名 harness 重编验证；首次 FAIL 不覆盖为 PASS。

用户已确认持续在场，并完成过两次左电机电源循环/物理 USB 拔插。后一次已用于本次
FAIL 的 D9 一次授权，不能复用或自动重试。S-45 软件分包提交并与 Orin 对齐后，再请
用户给新的左物理边界完成信号，执行 D9 一次授权 → H2 四组100+1000样本。
右臂物理步骤尚未确认。S-45 软件修复与验收已完成，当前没有 H2 进程运行；
新的左臂重置及持续值守问题已提出，尚待现场回复。

Orin `loosqk@10.0.1.40`，当前现场产物在
`target/post_e_hardware_current/s45/recheck/artifacts/release`；新驱动为
`target/post_e_h2_rt/s45/left_h2_current_driver.py`（默认只列计划，新 HEAD 与物理事实
缺一不可；构建 base=`a161a7b` 不反填后续提交）。
普通 loosqk rtprio99/memlock无限，NOFILE1024；需要权限直接 sudo，但其限额不同。
工具在 Tokio 创建前 `taskset -c 0-3`，serial FIFO70，左CPU4/右CPU5，其余OTHER0@0–3。
实际内核 CONFIG_PREEMPT=y，不称 PREEMPT_RT。真实串口只按已核 by-id/D2 身份选择。
后续硬序：通信硬化现场验收 → D15退役决策 → Rerun桥 → F7判据小件。
