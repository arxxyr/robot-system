# 02 · Claude Code 上手指南

> 给在本仓库开启新 session 的 agent 读；路径以实际 checkout 为准。

---

## 你接手的是什么

`robot-core`（工作代号 `robot-system`）—— 一个 **Rust 通用机器人运行时**，三层架构（S0 反射 / S1 快反射 / S2 慎思），本机通信走 iceoryx2 + FlatBuffers，对外走 gRPC。

目标：替换上游 dimos 项目（Python 框架）的运行时部分，同时保持可跨机器人复用（XLeRobot、Piper、移动底盘、未来四足 / 人形）。

用户是**追求完美的 Rust / C++ 程序员**，见 `~/.claude/CLAUDE.md`（中文交流、commit 带 emoji、commit 前必 fmt + clippy 零警告、不加 Co-Authored-By）。

---

## 当前状态

2026-09-22本次左H2四组各100预热+1000正式通过，runtime `c5daa67` / build `fb600fd`。
真实收尾、工具/driver回收及清场已独审/root验收，本轮已收口10cff7a；旧三次FAIL与前置拒绝保留。
本轮值守已解除，电源UNKNOWN；右未测、未安装、D15 OPEN。右工具/W1已合并`5952498`，
Orin新来源绑定及64Python/默认并行latch10、唯一mock4×2已通过独审；详HANDOFF。
本轮软件证据已归档；用户暂不能在场，只继续软件准备，右硬件不启动。按最新要求持续推进至周额度剩15%，每阶段验收commit，不自动物理重试或push。
完整数据、第三组max与P99区别及来源见[HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。

以下为已收口`eb59bcc`的历史真实H2失败记录，不覆盖本次新运行结论：

2026-09-22在b4ff720与新现场“本次全部就绪”确认下，执行一次真实左H2：RT门通过，
topic-idle 100次预热+1000正式PASS；topic-inflight 100次预热+639正式后FAIL。
seq13681注入请求3895634ns、实测3894208ns，短1426ns，原严格判据未放宽。
authorize/launch各1，工具/driver exit1且真实reap；独立断扭、最终清场成功，双marker Dirty。
第3/4组未执行；本次失败证据收口提交后暂停检查，不自动重试或push。电源未有收尾后新确认。

此前同日取得现场事实后，一次driver在TRUST权限前置exit1/reap，
authorize/launch均0，未访问电机。修复1b6db3e/4191bf0共用身份/RT/TRUST/进程预检，
有限固定sudo只读补证仍拒绝未知；合法零启动计数修复保完整PID/start/boot/UID绑定。
50项Python、Orin真实sudo集成及新HEAD生产4/测量3、7help、15收尾、只读复检通过。
首只读复检FAIL与原H2拒绝保留，详HANDOFF；该软件阶段收口b4ff720，未安装产物。
以下P.7基线与旧硬件记录为历史来源，不能视作当前电源状态或新H2通过。

```
设计版本:     v0.1.2 已冻结（docs/00_design.md）
实施阶段:     v0.5 候选 E 受监督功能 demo 完结；D15 硬件 fail-safe OPEN
当前候选:     post-E 通信故障硬化，Spec Approved（2026-09-05 round 21）
当前步骤:     左H2收口10cff7a；5952498原生软件验证/独审通过，证据已归档
当前授权:     Astra/medium开发及独审，连续推进至周额度剩15%；每阶段验收commit，不push
执行入口:     team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md
平台:         左 H1/H2/H3 PASS；三次旧H2 FAIL及一次前置拒绝保留；右臂未测
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

W3/W4及RR底层已接通高/普通双队列、唯一writer、RPC责任账本和受监督关闭链。
R4c监督退出与真实重启已提交1b0feff；R4d-A原连接真竞争已提交f9a85be。
R4d-B在同一A源码基线上完成双主机软件验收：570编译输入一致，未改生产/测试
源码；本机14 fresh/53按目标复用，Orin按五轮实际来源覆盖67有效门。原四FAIL
保留，原生26报告/19新增profile实例及真实manager场景通过，独审/root已验收。
B已提交cdeddd2并推送origin；P-E两端同571编译输入各All72全过，4096原FAIL/exit1
严格派生EXPECTED_RED；8192本机360.459s/8298新写、Orin360.087s/8270新写，运行期
GC分别1391/1379。完整收尾与独审/root验收通过，已提交2f1de02，仅软件。P.7新驱动
和产物校验37测试通过，源码8491b42/c759082/b7cbaca；Orin原生生产4/测量3及7help
通过，15次真实完成及进程清场已核。首轮PATH启动失败保留，不称单轮全过。
已审操作单见 scripts/systemd/README_dimos_replace.md。
Orin实际安装件位于target/release，禁止在那里构建候选；独立构建目录先冻结生产四件，
再构建测量三件。只读盘点不等于已安装、已授权marker或取得新的现场事实。
最新指令为持续推进至周额度剩15%，替代原25%停止线；不再每阶段自动暂停；每阶段必要验证、独审/root
验收和commit仍保留，新提交不自动push。真实H2须另核当次物理重置、支撑与持续在场事实，不能自动重试。
W1父用例隔离测试已本地集成验证；旧偶发失败具体根因仍OPEN_UNCONFIRMED。
原门/原件、恢复过程和精确身份均以HANDOFF/H包为准，不将分批结果称单轮全过。

左H1/H2/H3通过，三次旧H2 FAIL与前置拒绝保留；当前只接受本次新物理边界的左H2结果。
六服务停止、串口无占用、双marker Dirty；值守解除但电源UNKNOWN。右未测、正式部署未做，
D15 OPEN、deployable=false。新物理动作仍需当次事实，不能复用已消费授权或输出目录。
Orin调试/RT必要时直接sudo；标定沿已核Python JSON。新版5952498不混作已结束左H2的build来源；本轮native及integration证据已归入H包，本轮post/commit后生件仍在包外。

证据唯一入口为 [H.json](../.verify/post_e_comm_fault_hardening/H.json) 和
[H_evidence.tar.gz](../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)；C5独立
基线保留C5.json/C5_evidence.tar.gz。精简前入口文档保存在H包W3/root_closeout，
旧失败实件保留；后续通过不能回填旧来源。W4/R4及P-E双环境软件验证已完成；不外推额外内存序证明，不称优先级改动已通过硬件长负载。

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
