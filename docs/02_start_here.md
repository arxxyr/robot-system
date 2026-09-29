# 02 · Claude Code 上手指南

> 给在本仓库开启新 session 的 agent 读；路径以实际 checkout 为准。

---

## 你接手的是什么

`robot-core`（工作代号 `robot-system`）—— 一个 **Rust 通用机器人运行时**，三层架构（S0 反射 / S1 快反射 / S2 慎思），本机通信走 iceoryx2 + FlatBuffers，对外走 gRPC。

目标：替换上游 dimos 项目（Python 框架）的运行时部分，同时保持可跨机器人复用（XLeRobot、Piper、移动底盘、未来四足 / 人形）。

用户是**追求完美的 Rust / C++ 程序员**，见 `~/.claude/CLAUDE.md`（中文交流、commit 带 emoji、commit 前必 fmt + clippy 零警告、不加 Co-Authored-By）。

---

## 当前状态

独立 `robot-viz` 与隔离观测镜像已完成软件验收，文件/实时 Viewer 与最终 Linux 全仓回归均通过。
当前完成状态以 [V1–V8](10_verification.md) 和[证据摘要](../.verify/robot_viz_2026-09-29/summary.json)为准。
右 H2、D15 和正式退役保持原结论；本次只推进软件，不需要重做硬件动作。

2026-09-29后续已做双臂各一次腕部小幅真机探针并完成断扭收尾；头部/底盘未测主动运动。
已按用户要求加入穷尽类型分类，本机147＋5、隔离Linux ARM根Clippy及167＋42测试通过；
历史34条Unknown不回填原因，Orin回归仍待SSH信任核实。精确当前状态见[HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)
和[硬件检查](handoffs/hardware_smoke_2026-09-29_summary.md)；下段为同日先前软件阶段。

2026-09-29用户重新要求按当前进度继续开发，并明确允许在Orin编译测试。最终Disable延迟2.1秒的真实tonic回环、工具构建及绑定测试peer的Orin串行workspace已通过，原S1默认并行两例失败仍待查；完整验收结果见[HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。下方暂停指令仅为9月25日历史状态；本轮在独立目录进行纯软件验证，右H2仍整体FAIL，D15 OPEN、deployable=false，新的硬件动作仍需当次现场事实与独立GO。

2026-09-25交接：2026-09-23右H2四组各100预热+1000正式测量文件PASS，但最终非dry-run Disable约2秒`Cancelled: Timeout expired`，原run/report整体FAIL；authorize/launch各1，工具/driver/SSH exit1真实回收，独立只读清场PASS。右双rx marker与右EStop latch均`1\n`；清场时电源UNKNOWN，用户先确认断电、后报告已通电且电源/USB均重插，**本次交接时物理电源现状未知**；支撑/接触区未重新确认，不构成新H2授权。D15 OPEN、deployable=false。首USB瞬态FAIL、漏传alignment前置FAIL及其内部cleanup FAIL保留。软件修复`ca14f0e`已提交推送，Orin源码ff-only对齐；原生生产4/测量3、mock AC-12及定向单测仅软件通过，未做2.1秒延迟回执端到端、正式安装或硬件重测。用户要求暂停开发，仅本次handoff提交推送；完整原件见HANDOFF和H包。下段右H13状态仅为该历史阶段事实。

### 历史右H13阶段（2026-09-23，当时状态）

2026-09-23右H1/H3各100预热+1000正式测量通过，运行HEAD `3790fd9` / 实际build `5952498`；两工具真实回收与清场通过，证据已独审/root验收并归档。
首readonly命名冲突和setsid前置拒绝FAIL保留；本次经新GO执行，不称单轮全过。值守已解除、电源UNKNOWN。
左H2历史`c5daa67` / `fb600fd` PASS不变；右H2未测、未正式安装、D15 OPEN、deployable=false。
用户已撤销15%停线并确认本次右现场就绪；每阶段验收commit，不自动物理重试或push。下一物理步骤仍须对应新事实及授权。
完整统计、原件身份及历史失败见[HANDOFF](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。

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
当前步骤:     最终Disable延迟回执软件回归已验收；硬件复测需新准备，右H2仍整体FAIL
当前授权:     2026-09-29继续开发及Orin独立目录纯软件编译测试；阶段验收后commit，不push
执行入口:     team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md
平台:         左 H1/H2/H3 PASS；右 H1/H3通过、H2整体FAIL；所有历史FAIL保留
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
2026-09-25指令为仅提交推送当次handoff，之后暂停开发；2026-09-29已重新授权继续开发，当前范围见页首。
真实H2须另核当次物理重置、支撑与持续在场事实并取得独立GO，不能自动重试。
W1父用例隔离测试已本地集成验证；旧偶发失败具体根因仍OPEN_UNCONFIRMED。
原门/原件、恢复过程和精确身份均以HANDOFF/H包为准，不将分批结果称单轮全过。

左H1/H2/H3与右H1/H3通过，旧H2 FAIL及前置拒绝保留；右H2本次四组测量文件PASS但final Disable超时，整体FAIL。
最后一次软件验收时六服务停止、串口无占用、正式安装未变；右marker与EStop latch均Dirty。清场时电源UNKNOWN，后到用户答复通电与USB/电源重插；本次交接时物理现状未知，须重新核实。
D15 OPEN、deployable=false。新物理动作仍需当次事实与独立GO，不能复用已消费授权或输出目录。
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

### Rerun 观测工具（当前开发入口）

完成状态看页首及 [V1–V8](10_verification.md)，以下命令不启动任何控制服务。
桥只打开发布者创建的 `/_robot_observe/v1/...` 镜像；既有旧二进制没有这些镜像，
需后续按独立部署流程更新发布者才会出现真实数据。本轮没有更新 Orin 正式安装件。

```bash
# 使用现有 nightly；观测工具的专用 profile 保留 unwind。
cargo build --locked --profile release-tools -p robot-viz

# 与本机发布者处于同一共享内存环境，录制有限会话。
./target/release-tools/robot-viz \
  --config configs/xlerobot_left_arm.toml \
  --config configs/xlerobot_right_arm.toml \
  --save /tmp/robot-observation.rrd --duration-ms 10000 --max-frames 1000 \
  --report /tmp/robot-observation.json

# 同版外部 Viewer 打开记录；无需向机器人进程引入完整 Viewer。
uvx --from rerun-sdk==0.38.1 rerun /tmp/robot-observation.rrd

# 实时查看：先在一个终端启动仅监听本机的 Viewer。
uvx --from rerun-sdk==0.38.1 rerun --bind 127.0.0.1 --port 9876

# 再在另一个终端连接；默认持续到 Ctrl-C。
./target/release-tools/robot-viz \
  --config configs/xlerobot_left_arm.toml \
  --config configs/xlerobot_right_arm.toml \
  --connect rerun+http://127.0.0.1:9876/proxy \
  --report /tmp/robot-live.json
```

文件模式默认 10 秒或 1000 帧，单文件最多 10000 帧；`--duration-ms` 与 `--max-frames`
可显式缩小会话。实时模式没有默认总时长限制，断连或 SDK 无进展会报错收尾。
使用 `robot_time` 查看源采样时间；`published_time`、`received_time` 和整数 `source_seq`
保留发布与接收关系。图像、点云和张量支持仅表示转换能力，当前并不代表生产相机已接入。
`velocity_wire`、`effort_wire` 和 depth16 保留源值，不补造速度、力矩或深度的物理单位。
镜像允许丢失，缺包/拒绝/陈旧须结合报告判断，画面不能作为控制执行或物理安全确认。
默认曲线按单位分组；媒体按来源独立显示。当前官方 Viewer 默认字体缺少中文字形，
视图标题与固定状态采用 ASCII，中文原始事件仍原样保存。`OBSERVATION_ONLY` 表示只读观测，
`NO_DATA` 表示没有有效数据，`STALE` 表示源时间陈旧；`UNKNOWN_NOT_SAFE` 不提供物理安全确认。
张量与点云强度按当前帧的有限数值范围呈色，不是跨帧固定量程。支持的 CPU 张量数值
统一无损转为 f64 记录，原 dtype 留在来源描述中；呈色不会改写这些数值。
超出 GPU 预览能力时显示 `GPU_PREVIEW_LIMIT`，不能把可记录的 f64 数据等同于 GPU 保真显示。
同一 topic 的多个发布者持续积压时，底层接收从首连接扫描，其他实例可能长期不可见。
序号缺口只覆盖已观察到的实例，不能发现从未收到的发布者，也不能作为完整事件审计。

官方 Viewer 加载文件是异步过程。自动截图前应确认 recording 与视图已经打开；
仅 `rrd verify` 成功或截图命令退出为 0，都不能证明画面正确。
Viewer 的 `--save` 接收器被 SIGINT 中断后可能缺少文件 footer；这种临时捕获与桥自身
正常关闭的 `--save` 文件分别验收，不能把传输成功当作完整文件验收。

显式跨进程软件回归入口（无硬件）：

```bash
ROBOT_VIZ_TEST_ARTIFACTS="$PWD/target/robot-viz-evidence" \
  cargo test --locked -p robot-viz --test observer_roundtrip \
  -- --ignored --test-threads=1 --nocapture
```

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
