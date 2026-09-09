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
当前步骤:     C1–C5/S-45与S-48 A1/A2/A3a/A3b/W0/W1/W0.1已提交；W2a已提交b5a0bfa，W2b已评审提交6defac4，S46-B已评审提交5abd148，P-M已评审提交0d3caca，P-T已独立验收待提交，下一包P-S1/P-L1；S-46 Orin wire4满FAIL，双队列与生产总装仍待完成
执行入口:     team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md
平台:         左 H1/H3 PASS；H2两次FAIL，现场已确认左臂断电；右臂未测
```

用户已于 2026-09-08 授权对齐文档并由专职 developer / reviewer 持续推进。
用户追加：每阶段评审与必要质量门通过即 commit，再推进下一阶段。
developer 可根据任务需要自主启动子 agent 并负责整合；reviewer 保持独立评审。
共享工作区按文件明确归属。用户2026-09-09更新分工：主agent负责方案和评审，
cc-claude优先负责具体开发与测试；其额度耗尽后，按用户混编授权由原生implementation
独占源码与Cargo，独立Codex评审；评审通过仍由主agent提交。
由团队完善函数级计划、独立评审通过后实施；原 9 月 5 日“仅准备 Plan outline”
是历史授权状态，不作为本轮停工理由。实现、验证、真机执行分别记录，不能将
Spec Approved 或旧测试记录写成新代码已通过验收。

C5 源码 `e6d2b306…2598`（308 输入）已获独立
**APPROVED_SOFTWARE_SCOPE / SOFTWARE_ACCEPTED_HARDWARE_PENDING**。本地与 Orin
均完成 **1737 PASS / 11 ignored**、67 软件场景和八套 smoke；四项 RT guard、
原 1000 轮精确 Node 目录门、fresh 与四个无 hook 正式产物均已核验。
完整计数、验收边界与当前证据见 [10_verification](10_verification.md)，
复现命令和实现约束统一见 [PLAN](../team-swe/specs/post_e_comm_fault_hardening.PLAN.md) P.6。

S1 的私有 `EventsPublisherGuard` 保证 publisher 先于 Node 退场；工具只按两个
存活 PID 的精确 Node 集合验收，baseline 每端非空，原 2 秒 grace 后不得新增目录，
空目录和同数量替换均失败。公开 spawn、QoS/history/retry/grace 不变。
工具 optional 依赖 `iceoryx2-bb-posix =0.9.3` 只在 pubsub feature 开启，复用现有 lock。

Orin 原仓库已更新，六项开发服务经授权按 gateway → S1 → S0 停止并保持
inactive/PID0。两端正式无 hook 产物已恢复，`deployable=false`，不自动启动服务。
Orin 调试 / RT 需要权限时直接使用已获授权的 `sudo`，不放宽资源阈值或测试预算。
左 H2 三进程 RT 检查已通过；首组 35 次在途分类失配后，epoch 10072 缺少 acquire，
严格分析器判 FAIL，正式样本 0；真实断扭确认、进程全部退出、marker 为 `1/1`。
S-45 / PLAN P.6.11 已修复采样边界、显式 idle 前置条件及工具跨读取误判；两端各
67 软件场景通过并独立批准，已提交 `965bcd7`；新物理边界确认后再执行 D9 一次授权。启动夹具
缓存旧路径的失败也已定位并按原预算复验通过。左 H1/H3 PASS、右臂未测、D15 `OPEN`。

2026-09-09在`ce61623`上重测左H2，topic/idle为100预热+138正式、零失配后缓存
恰4096项耗尽；另11条背景请求因串行await慢RPC导致启动迟到至少50ms。最终
GetState确认state3，随后Disable容量拒绝，整体仍FAIL；全部进程退出，用户确认
左臂断电、双臂继续支撑。S-46 / PLAN P.6.12只修工具负载、有限缓存配置和收尾留证，
先做两端同负载长测，再独立评审提交；不重复执行已消费的`left_h2_current`目录。
两臂继续直接读取Orin上既有Python标定JSON，名称/ID/字段兼容已只读核对。

S-48按用户要求分高/普通两个有界队列，每轮先高后普通、每项非阻塞推进。
A1先独立实现Clear文件事务与恢复，已通过28个模块入口、7项公开API验收、
24项旧latch回归和3项编译失败文档测试；全workspace fmt/Clippy零警告。
A2独立原子权限模块已通过48项模块测试、13项compile-fail、16项默认公开验收，
debug/release各21项真实guard验收与A1回归；root核验源码和原始证据后批准。
A2已提交`dee67fa`。A3a仅将该模块归位robot-core并由s0重导出同一模块，
方便后续adapter交接强类型租约；18道本地门通过，root已核语义等价、唯一计数器、
生产feature边界及源码身份。完整计数见10_verification与H包，不重复相加。
A3b快照并发修复通过22道本地门及独立源码/证据评审，已提交`d67752d`。
A3c/W0文件凭证已提交`8624f27`，W1启动锁与恢复已提交`1b21457`，W0.1只读双份核验
已提交`81c5c54`。W2a独立writer模块已通过本地七门与独立源码终审，root另核完整
344项来源及逐项日志；已修复阶段戳/公告顺序、断链观测与测试资源和并发问题，
W2a已提交`b5a0bfa`。W2b关闭/join机制已通过本地十一门和独立终审，346项来源一致，
workspace1914通过、1既有ignored；初版源码问题已修，详H包local/s48/w2b/review.md。
W2b已提交`6defac4`，S46-B已评审提交`5abd148`，P-M已评审提交`0d3caca`，P-T已独立验收待提交，下一包P-S1/P-L1。W3a协调原语方案已批准，见PLAN P.6.14.13。
新权限尚未成为生产权威，双队列与唯一磁盘writer尚未总装；不得称优先级问题已修复。
当前分工、最新门与提交证据统一见HANDOFF。

C5 证据保留 [C5.json](../.verify/post_e_comm_fault_hardening/C5.json) 与
[C5_evidence.tar.gz](../.verify/post_e_comm_fault_hardening/C5_evidence.tar.gz)；
当前现场阶段另见 [H.json](../.verify/post_e_comm_fault_hardening/H.json) 与其唯一证据包。
旧轮次日志、快照、重复产物和历史归档按用户授权清除。

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
