# 02 · Claude Code 上手指南

> 给在本仓库开启新 session 的 agent 读；路径以实际 checkout 为准。

---

## 你接手的是什么

`robot-core`（工作代号 `robot-system`）—— 一个 **Rust 通用机器人运行时**，三层架构（S0 反射 / S1 快反射 / S2 慎思），本机通信走 iceoryx2 + FlatBuffers，对外走 gRPC。

目标：替换上游 dimos 项目（Python 框架）的运行时部分，同时保持可跨机器人复用（XLeRobot、Piper、移动底盘、未来四足 / 人形）。

用户是**追求完美的 Rust / C++ 程序员**，见 `~/.claude/CLAUDE.md`（中文交流、commit 带 emoji、commit 前必 fmt + clippy 零警告、不加 Co-Authored-By）。

---

## 当前状态

```
设计版本:     v0.1.2 已冻结（docs/00_design.md）
实施阶段:     v0.5 候选 E 受监督功能 demo 完结；D15 硬件 fail-safe OPEN
当前候选:     post-E 通信故障硬化，Spec Approved（2026-09-05 round 21）
当前步骤:     R4b1已验收提交b4b44f7；R4b2正式28门及独审/root/归档通过；提交后进入R4b3的25点
当前授权:     继续有效开发至周额度剩20%；每阶段验收后提交，不push
执行入口:     team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md
平台:         左 H1/H3 PASS；H2两次FAIL，现场已确认左臂断电；右臂未测
```

用户已于 2026-09-08 授权对齐文档并由专职 developer / reviewer 持续推进。
用户追加：每阶段评审与必要质量门通过即 commit，再推进下一阶段。
developer 可根据任务需要自主启动子 agent 并负责整合；reviewer 保持独立评审。
共享工作区按文件明确归属。用户2026-09-14更新分工：主agent负责方案和最终评审，
原生子agent负责开发与测试，另派独立评审；开发者可按文件继续拆分，Cargo由主开发
统一执行。此分工取代此前cc-claude优先安排，评审通过仍由主agent提交。
由团队完善函数级计划、独立评审通过后实施；原 9 月 5 日“仅准备 Plan outline”
是历史授权状态，不作为本轮停工理由。实现、验证、真机执行分别记录，不能将
Spec Approved 或旧测试记录写成新代码已通过验收。

W3已接通共享权限、高/普通双队列、唯一文件writer、RPC责任账本和受监督关闭链。
W4a新增Created/Unsynced真实文件故障恢复验收：373源码/187输入首末一致，
15门满足期望、新增2/2与原All72/72，独立源码/证据及root验收通过。
旧编译/Clippy失败与汇总脚本更正均保原件；准确统计见HANDOFF、H包和10_verification。
W4b固定五例SIGKILL/同目录恢复已独立源码/证据及root验收，376源/129输入一致；
16门满足期望，新5/5、旧W4a2/2及All72/72通过。已提交a2598a5。原暂停已由2026-09-14继续目标及额度重置说明结束。
RR r3的析构和公用CAL遗留问题已获方案ACK；vendor/模型/迁移另包，不外推至Orin或电机。

R0以两个0.9.3原发布包接入唯一path依赖图，保14个本方成员与其余锁定版本；
仅补一处当前nightly原子API别名兼容。fmt、三组Clippy、构建及transport99/RPC304通过，
来源与运行证据已独审。R1已实现CAL正常关闭/清理区分及真实入队回执，同源模型6项通过；
正式21门全0，CAL45、工作区2207、S0 hook656通过，原9项ignored保留。
根/嵌套两vendor运行时闭包80包身份一致，Loom不进生产图。R2已验收提交06fcf0b。
R3已直接替换本方RR工厂并成对联动真实peer，37门与538输入身份核验通过，
dev/release各W4a2/W4b5/All72通过；独立源码/证据及root已验收本地范围。
完整T8/SUP/ENV和双环境仍待R4；上述本地结果不代表Orin或P-E通过。

左H1/H3通过、H2两次FAIL；用户最后确认左臂断电、双臂支撑。右臂未测，
六服务最后停止，wire4历史FAIL、D15 OPEN、deployable=false。R4准备已只读盘点Orin
软件环境与旧H2源码残留，未更新电机现场状态；来源见rr_r4/environment_inventory.md
及orin_residual_source_review.md。Orin调试/RT必要时直接sudo；标定使用已有
Python JSON。新H2须先完成软件验收/提交/两端对齐，再取得新物理重置与在场事实，
不得复用已消费的left_h2_current目录或旧驱动。

证据唯一入口为 [H.json](../.verify/post_e_comm_fault_hardening/H.json) 和
[H_evidence.tar.gz](../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)；C5独立
基线保留C5.json/C5_evidence.tar.gz。精简前入口文档保存在H包W3/root_closeout，
旧失败实件保留；后续通过不能回填旧来源。W4/P-E尚未完成，RR跨原子可见性
证明另列待办，不称优先级改动已通过硬件长负载。

---

## 文档阅读顺序

**首次进入 session 必读（按顺序）**：

1. `CLAUDE.md`（本项目工作约定，包括禁用 API 清单）
2. 本文档 `docs/02_start_here.md`
3. `docs/00_design.md`（冻结设计，**权威**）
4. `docs/01_implementation_steps.md`（v0.2 历史实施步骤，已收口）
5. 当前候选 Spec 与 HANDOFF（当前执行依据）；正式 Plan 随评审更新。

**用户通用偏好**：`~/.claude/CLAUDE.md`（已由 Claude Code 自动加载）

**冲突裁决**：`00_design.md` > `CLAUDE.md` > 全局 CLAUDE.md > 本文档。

---

## 硬性约束（破一条都不行）

1. **不改 `docs/00_design.md`**。架构已冻结。发现语义问题 → 写 `docs/CHANGELOG.md` 讨论，不直接改设计。
2. **不跳当前候选的依赖与验收门**。v0.2 十步已收口，当前按批准的 Spec / Plan 推进。
3. **commit 前零警告**：`cargo fmt --all --check && cargo clippy --all --all-targets -- -D warnings`。
4. **Rust nightly 是硬要求，严格不用 stable**（对应 `docs/00_design.md` §9）。**核心 crate**（`robot-core` / `robot-msg` / `robot-transport` / `robot-s0` / `robot-s1`）**原则上不写 `#![feature(...)]`**，目的是降低 nightly 升级回归风险（不是为了留 stable 后门）；确需启用必须 crate 顶部注释 + review。
5. **业务代码禁用 API**：
   - `std::time::Instant::now` / `SystemTime::now` / `tokio::time::Instant::now` → 走 `robot-core::time::RobotClock`
   - `google.protobuf.Any` 作为 RPC payload → 用 `CommandMeta` + 强类型 Command
6. **RT thread 禁令**（设计文档 §0）：禁 heap alloc / `Arc::clone`、`Arc::drop` / 文件 IO / 网络 IO / `panic!` / `format!` / 带 format arg 的 `tracing::info!`。
7. **commit message 不加 Co-Authored-By**（全局 CLAUDE.md §4）。

---

## 当前任务：post-E 通信故障硬化

1. 先看工作区和当前交接，保留已有改动；功能分支为 `feat/post-e-comm-fault-hardening`。
2. 阅读 [`Spec`](../team-swe/specs/post_e_comm_fault_hardening.md) 的已批准 D1–D15、
   AC-1–24 / AC-Q 与边界，再核对函数级 Plan 的真实代码落点。
3. 专职 developer 实施，专职 reviewer 独立评审 / 验收；主 agent 维护文档、门禁与整合。
   共享文件始终只有一个写者，Cargo 长检查统一调度。
4. 内部 Plan 评审通过后分包实现、回修；所有工作包与测试通过后才记录候选完成。
5. 本候选不含 RX 重同步 / 串口 reopen、D15 硬件 fail-safe、跨臂独立联锁；
   不把本地开发授权解释成真机上扭或正式退役授权。

---

## 工作方式

- **先核事实再推进**。围绕当前任务报告进度、风险与验证证据。
- **先读后写**。修改任何文件前先 `Read` 一次。
- **小步 commit**。一个逻辑单元一个 commit，不要攒一大堆。
- 例行实现细节由团队核码并评审；真正改变已批准语义时先 Reverse Sync，不私改冻结架构。
- **不主动创建 README.md / 文档** 除非用户明确要求（全局 CLAUDE.md）。
- **保持当前状态入口一致**：更新 CLAUDE.md / 当前 Spec / HANDOFF；历史文档只补明确状态指针，不覆盖历史证据。

---

## 上游参考（dimos 项目）

上游 dimos / dimos-deploy 是已验证 adapter 逻辑与功能基线的只读参考，路径以本机 checkout 为准。
候选 E 已跑通控制执行面受监督 demo；旧栈自启已 disable、unit 保留回滚，尚未正式退役。
需要硬件移植依据时可查上游：

| 用途 | 路径 |
|------|------|
| Feetech STS3215 寄存器序列规范 | `docs/06_lerobot_replace_spec.md` |
| 自研 Python STS3215 驱动（移植参考） | `src/xlerobot_dimos/common/feetech_io/` |
| 弧度 ↔ raw / 三轮全向运动学 | `src/xlerobot_dimos/common/units.py` |
| 校准 JSON 示例 | `calibration/` |
| URDF + STL（未来 IK / 3D 渲染） | `assets/xlerobot/` |
| Phase 6 OpenPI + RL 集成（S2 参考） | `docs/07_phase6_openpi_rl.md` |

**这些是参考，不是依赖**。本项目不 import Python 代码，要**移植成 Rust**。

---

## 和 dimos 仓库的关系

- 本项目是**独立 git 仓库**，与 dimos 无 git 联系。
- 设计文档源版本在 dimos `docs/09_robot_framework.md`；本项目的 `docs/00_design.md` 是**复制快照**，不反向同步。
- 如果在本项目发现需要改设计：先在本项目 `docs/CHANGELOG.md` 记录，评审通过后由用户决定是否同步回 dimos 侧。
- 候选 E 的已批准路线是原生替换控制执行面；最新边界见 `handoffs/v0.5_dimos_buyin_decision.md`，
  不沿用早期“改造 dimos 为 RPC 客户端”的过期计划。

---

## 禁止讨论的话题（架构已冻结）

遇到以下话题直接引用 `docs/00_design.md`，不展开：

- iceoryx2 vs eCAL / DDS / Zenoh 机内替代
- FlatBuffers vs Cap'n Proto / protobuf
- gRPC vs ZMQ / HTTP
- S0/S1/S2 重分层或合并
- Python 是否能进 S1 主进程
- SoftRT 是否能挂 `s0_pub_thread`
- `layout_id` 宽度（已定 u64）
- `panic = "abort"` 范围（已定仅 S0/S1）
- `CommandMeta` 是否用 `Any`（已定强类型）

要改 → `docs/CHANGELOG.md` + 用户评审。

---

## 常见陷阱

- 在 `robot-core` 里顺手加 `tokio` / `tracing-subscriber` 依赖 → 破坏"零 runtime 依赖"铁律（§4.2）。
- RT thread 里写 `if debug { tracing::info!(...) }` → 日志 format 参数构造仍分配。
- `ReloadCalibration` 用 `Arc::clone` → 破坏 §4.9 硬约束，必须走 `RtSnapshotReader/Publisher`。
- `layout_id` 被手写成 `u32` → v0.1.2 已升 u64，所有 schema 字段也是 `ulong`。
- 发 RPC 命令不带 `CommandMeta` → 破坏幂等性保证。
- commit message 不带 emoji / 用 Co-Authored-By → 全局 CLAUDE.md §4 硬性规则。

---

## 结束 session 前

- 确认 `cargo fmt + clippy` 零警告
- 确认当前 step 的 "退出标准" 全部达成再宣称完成
- 确认当前 Spec / HANDOFF 的实现状态、评审结论和证据指针已更新
- 如果还没到下一步自然的 commit 边界，不要留未 commit 的大改
