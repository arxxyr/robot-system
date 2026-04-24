# 文档导航

| 文件 | 用途 |
|------|------|
| [`00_design.md`](00_design.md) | **v0.1.2 冻结设计**（权威，架构不再改动） |
| [`01_implementation_steps.md`](01_implementation_steps.md) | v0.2 实施步骤（10 步强制顺序，执行依据） |
| [`02_start_here.md`](02_start_here.md) | Claude Code 上手指南（新 session 必读） |
| [`10_verification.md`](10_verification.md) | 验证测试文档（活文档，Step 5 完成时落骨架，随后每 Step 填实测） |
| [`CHANGELOG.md`](CHANGELOG.md) | 偏离 `00_design.md` 冻结语义的登记簿 |
| [`targets/`](targets/) | 未来部署目标分析（XLeRobot / ATOM01 / Piper ...）；跟随上游演进更新，不影响冻结设计 |

**首次进入 session 阅读顺序**：`../CLAUDE.md` → `02_start_here.md` → `00_design.md` → `01_implementation_steps.md`

---

## Spec 分工

本仓库把 "spec（specification，规格 / 规范）" 按**职责**分成四类，互不重叠。**spec = 写契约、不写实现**；实现是 spec 的一个满足例，不是 spec 本身。

| Spec 类别 | 对应文档 | 写什么（契约） | 不写什么 |
|---|---|---|---|
| **Design spec** — 架构规格 | [`00_design.md`](00_design.md) | 不变量、语义契约、线程模型、消息格式、§6.7 five invariants | 行数 / 函数签名 / 调度细节 |
| **Implementation spec** — 实施规格 | [`01_implementation_steps.md`](01_implementation_steps.md) | 10 步顺序、每步交付清单、退出标准、可测性原则 5 条 | 具体怎么 coding / 代码风格 |
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
- **Spec vs CHANGELOG**：[`CHANGELOG.md`](CHANGELOG.md) 记录"spec 从 A 变到 B、原因是什么"；spec 本身永远是**当前版本**的样子。
- **Spec vs 指南**：[`02_start_here.md`](02_start_here.md) 是 onboarding 指南，不是 spec —— 可以随意更新、不走 CHANGELOG。

### 改 spec 的规矩

- **Design spec 冻结**：`00_design.md` 不在代码 PR 中改。发现语义问题 → `CHANGELOG.md` 登记 → 评审通过 → 该动才动。
- **Implementation / Verification spec 随步更新**：每完成一步同步 "当前位置" / 指标状态 ⏳→🟡→✅。
- **Target spec 跟随上游**：上游仓库演进时更新 `targets/*.md`，不影响 design spec。
