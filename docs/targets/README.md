# Deployment Targets（部署目标）

本目录汇总 `robot-system` 规划支持的具体机器人平台，以及每个平台上游（已有参考实现）的现状分析 + 迁移映射。

目的是让 framework（`robot-s0` / `robot-s1` / `robot-transport` / …）在设计和演进时**始终针对真实部署目标的硬约束**，而不是抽象出一个"通用框架"却没人跑得起来。

## 优先级

| 平台 | 上游参考 | 框架阶段 | 状态 | 文档 |
|------|---------|---------|------|------|
| XLeRobot | `dimos`（Python） | v0.4（首个 adapter 落地目标） | 规划中 | 见 `00_design.md` §11.3（暂不独立成 target spec） |
| ATOM01 人形 | `atom01_deploy`（ROS 2 + C++） | v0.5 候选（端到端 RL 推理） | 规划中 | [`atom01.md`](atom01.md) + [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md) + [`atom01_migration.md`](atom01_migration.md) |
| AgileX Piper | — | v0.6（异构验证） | 规划中 | 见 `00_design.md` §11.4 |
| 移动底盘 / 四足 / 其他人形 | — | ≥ v0.7 | 未立项 | — |

> 说明：**XLeRobot 当前不补独立 target 文档**，现役控制栈在 `02_start_here.md` 里有指针。第二个独立 target（如 Piper 实际立项时）会新建 `piper.md`，同 `atom01.md` 骨架，以逼出"骨架是否真可复用"的早期反馈。

---

## 每个平台文档的组织方式

### 主文件 `targets/<name>.md` —— 6 节骨架

| 节 | 性质 | 写什么 | 改动流程 |
|---|---|---|---|
| §1 硬件清单 | 事实 | 控制器 / 传感器 / 执行器 / 总线 / 内核 | PR 直接改（跟随上游硬件变更）|
| §2 训练 / 部署契约 | **★ target spec · 硬契约 ★** | 必须 1:1 复刻到 Rust 的不变量、数值常量、字段顺序、公式、相位 | **必经 `docs/CHANGELOG.md` 登记** |
| §3 上游实现参考 | 非 spec | 架构速写、线程模型、启动流程、服务清单、性能数据 | PR 直接改 |
| §4 在 robot-system 中的映射 | 设计映射 | 哪些走 S0 / S1 / policy host / 业务 app crate；clear ownership | PR 直接改（跟随框架演进）|
| §5 新增 adapter / crate 需求 | 工程规划 | crate 边界、trait 实现范围、RT 约束 | PR 直接改 |
| §6 风险与门槛 | 工程风险清单 | 精度、延迟、复现性、RT 违例、训练-部署契约 | PR 直接改 |
| §7 参考 | 外部链接 | 上游仓库、源码快照、协议手册 | PR 直接改 |

### 辅助文件

| 文件 | 性质 | 作用 |
|---|---|---|
| `<name>_migration.md` | **非 spec** 路线图 | "框架 ready 后这台机器人还差什么"的项目管理清单；自由更新，不走 CHANGELOG |
| `<name>_<subsystem>_<commit7>.md` | **源码快照**（commit-pinned） | 针对上游某个子系统的逆向工程笔记；文件名带 commit 前 7 位；**上游升版时新建文件而不 in-place 改**；迁移完成后可归档删除 |

---

## 哪些东西**进** target 文档

只进**上游源码 / 硬件 / 训练侧 不提供 spec**的、robot-system 做决策时要回看的内容：
- 硬件连接 / 总线 / CAN 映射
- 训练-部署契约的硬数值（obs 布局、scale、dt / decimation、关节顺序、软限位、kp/kd）
- 上游实现的算法路径（闭链公式、IMU 外参）
- 移植时的 clear ownership 边界

## 哪些东西**不进** target 文档

- 架构语义（去 `00_design.md`）
- 实施步骤 / 进度（去 `01_implementation_steps.md`；target 自己的路线图去 `_migration.md`）
- 指标 baseline / CI / env_id（去 `10_verification.md`）
- 语义变更记录（去 `CHANGELOG.md`）
- 单次调试经验 / 修 bug 记录

---

## Spec 登记规则（与 `docs/CHANGELOG.md` 的关系）

target 文档里 **仅 §2 训练 / 部署契约** 属于 spec。

| 改动 | 是否走 CHANGELOG | 示例 |
|---|---|---|
| 上游 obs_num 从 78 改成 82 | **必须** | §2.2 契约变了 |
| 上游 `gravity_z_upper` 阈值改 | **必须** | §2.4 倒地判定变了 |
| 上游 kp/kd 表换了 | **必须** | §2.7 契约变了 |
| 上游新增一个 ROS 服务 | 不需要 | §3.4 是参考，非契约 |
| 硬件从 Orange Pi 换 RDK X5 | 不需要 | §1 是事实，非契约 |
| 延迟预算数值更新 | 不需要 | §4.3 是设计映射 |
| 路线图加减步骤 | 不需要 | 在 `_migration.md`，非 spec |
| 源码快照过期 | 不需要 | 新建 `_<newcommit>.md`，归档旧 |

---

## 新增一个 target 的工作量

当你给 `robot-system` 加第二个真 target（例如 Piper 立项时）：

1. 建 `targets/<name>.md`，照 6 节骨架填（先只写 §1、§2 硬契约、§4 映射；其他节可先 stub）。
2. 若上游是闭源 / 复杂 ROS 包，可选做源码快照 `targets/<name>_<subsystem>_<commit7>.md`；若上游就是一份清晰 Python spec，不需要。
3. 若迁移是长期工程，建 `targets/<name>_migration.md`。
4. 更新本 README 的《优先级》表加一行。
5. 若新 target 发现 6 节骨架不合身 → 不要擅自改 `atom01.md`；先在本 README 评估是否改骨架，改了要更新所有已有 target（目前只有 atom01）。

---

## 冻结设计的位置

**不要**把部署目标写入 `00_design.md` —— 设计冻结，目标文档只是**对齐 reality 的参考资料**，可以随上游项目演进更新。
