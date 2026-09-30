# 文档导航

当前（2026-09-30）：H2 V2 的 `59120e34` 已在最新浮动 nightly 下通过 Orin 六条 release
配方、七冻结件及定向软件门，release/post/archive 独审与 root 软件验收通过，
253 份原件及本机证据已归档至[最终证据入口](../.verify/h2_nightly_v2_2026-09-30/summary.json)。
未重跑完整 workspace。2026-09-30 用户确认无独立自动断电保护，决定暂缓 D15 实施/验收，
保留 OPEN/DEFERRED，不记 PASS；受监督 demo 沿既有 D15-R 继续。下一步右 H2 完整复测，原 FAIL 保留。
供电分域未知，本轮不再追问、选型、采购或接线。
同日先前 Rerun 原生定向软件、最终桥复验、官方 RRD 核验及正式快照比对已验收归档，
见[Rerun 证据摘要](../.verify/robot_viz_orin_2026-09-30/summary.json)。
9/29 的 Rerun 文件/实时 Viewer 与隔离 Lima Linux 全仓回归已验收，不代表本轮重跑全仓。
使用方式见 [Rerun 入口](02_start_here.md#rerun-观测工具当前开发入口)，实际完成状态与边界见
[V1–V8 验收](10_verification.md)和[当前交接](../team-swe/specs/post_e_comm_fault_hardening.HANDOFF.md)。
正式 ca14 安装件未改，本轮未部署或执行新硬件动作；右 H2 整体 FAIL、D15 OPEN/DEFERRED、`deployable=false` 保持。
通信故障后续依据为 [post-E Spec](../team-swe/specs/post_e_comm_fault_hardening.md)，
[D15 工作单](../team-swe/specs/d15_hardware_fail_safe.CLOSURE.md)尚非批准规格或硬件实现。

| 文件 | 用途 |
|------|------|
| [`00_design.md`](00_design.md) | **v0.1.2 冻结设计**（权威，架构不再改动） |
| [`01_implementation_steps.md`](01_implementation_steps.md) | v0.2 历史实施步骤（已收口；当前执行依据见活跃候选 Spec） |
| [`02_start_here.md`](02_start_here.md) | Claude Code 上手指南（新 session 必读） |
| [`10_verification.md`](10_verification.md) | 验证测试文档（活文档，Step 5 完成时落骨架，随后每 Step 填实测） |
| [`CHANGELOG.md`](CHANGELOG.md) | 冻结后 spec 变更登记簿：①偏离登记 · ②回写登记 · ③Target 契约登记（仅 `targets/<name>.md` §2） |
| [`targets/`](targets/) | 未来部署目标分析（XLeRobot / ATOM01 / Piper ...）；跟随上游演进更新，不影响冻结设计 |
| [`handoffs/`](handoffs/) | 每次 session 结束的**进度快照**（非 spec，不走 CHANGELOG），异地 resume 用：`ls -t docs/handoffs/*.md \| head -2` |

**首次进入 session 阅读顺序**：`../CLAUDE.md` → `02_start_here.md` → `00_design.md` → `01_implementation_steps.md`

---

## Spec 分工

本仓库把 "spec（specification，规格 / 规范）" 按**职责**分成四类，互不重叠。**spec = 写契约、不写实现**；实现是 spec 的一个满足例，不是 spec 本身。

| Spec 类别 | 对应文档 | 写什么（契约） | 不写什么 |
|---|---|---|---|
| **Design spec** — 架构规格 | [`00_design.md`](00_design.md) | 不变量、语义契约、线程模型、消息格式、§6.7 five invariants | 行数 / 函数签名 / 调度细节 |
| **Implementation spec** — 实施规格 | [`01_implementation_steps.md`](01_implementation_steps.md)（v0.2 历史）与 [`team-swe/specs/`](../team-swe/specs/)（当前候选） | 候选范围、验收条件、接口与执行计划；v0.2 保留 10 步与可测性原则 | 已执行但未验证的结果不得写成 PASS |
| **Verification spec** — 验证规格 | [`10_verification.md`](10_verification.md) | 指标目标值、measured baseline、CI 串行顺序、env_id | 每一次实际跑出的原始数据 |
| **Target spec** — 部署目标规格 | [`targets/<name>.md`](targets/) 的 **§2 训练 / 部署契约** | 每个平台必须 1:1 复刻的硬契约（字段顺序、数值常量、公式） | 路线图 / 源码 walkthrough（见下方辅助文件） |

> **Target spec 的范围很窄**：只有 `targets/<name>.md` 的 §2 是 spec；同文件的 §1 / §3 – §7 跟随上游演进更新，不走 CHANGELOG。
>
> 附属文件（**非 spec**）：
> - `targets/<name>_migration.md` — 路线图 / 项目管理清单
> - `targets/<name>_<subsystem>_<commit7>.md` — 上游某 commit 的源码快照（commit-pinned）
>
> 详见 [`targets/README.md`](targets/)。

**外部 spec 的引用**：Step 10 引用 dimos `docs/06_lerobot_replace_spec.md`（Feetech 寄存器序列规格）—— 算 target spec 的一部分，不进本仓库。

### Spec vs 其他文档

- **Spec vs 代码**：spec 先于实现、可在没任何代码时评审拍板。
- **Spec vs CHANGELOG**：[`CHANGELOG.md`](CHANGELOG.md) 记录"spec 从 A 变到 B、原因是什么"；活跃候选 spec 本身保持当前契约，明确标为历史的实施计划保留原时点内容。
- **Spec vs 指南**：[`02_start_here.md`](02_start_here.md) 是 onboarding 指南，不是 spec —— 可以随意更新、不走 CHANGELOG。

### 改 spec 的规矩

- **Design spec 冻结**：`00_design.md` 不在代码 PR 中改。发现语义问题 → `CHANGELOG.md` 登记 → 评审通过 → 该动才动。
- **活跃 Implementation / Verification spec 随步更新**：每完成一步同步实现 / 指标状态；已归档阶段只补当前入口指针，不重写历史计划与结果。
- **Target spec 跟随上游**：上游仓库演进时更新 `targets/*.md`，不影响 design spec。**§2 契约节改动必经 [`CHANGELOG.md`](CHANGELOG.md) 第 3 类（target-contract）登记**；其他节 PR 直接改。规则表见 [`targets/README.md`](targets/)《Spec 登记规则》。
