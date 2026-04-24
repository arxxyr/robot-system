# ATOM01 迁移路线图（非 spec）

> ⚠️ **本文不是 spec**
> - 这是**项目管理 / 路线图**，随项目推进自由更新，**不走** `docs/CHANGELOG.md`；
> - 真 spec / 硬契约去 [`atom01.md`](atom01.md) §2《训练 / 部署契约》；
> - 源码级快照（commit-pinned）去 [`atom01_inference_node_d983373.md`](atom01_inference_node_d983373.md)；
> - 顺序不是 `robot-system` 的 v0.x 版本顺序，只描述"框架就位后这台机器人还差什么"。

---

## 前提条件

- `robot-system` v0.4 – v0.5 框架已就位：`robot-s0` 四线程闭环、`robot-transport` / iceoryx2、`robot-rpc`、`robot-policy-host` 均落地。
- 见 [`docs/01_implementation_steps.md`](../01_implementation_steps.md) Step 1 – Step 10 全部 ✅。

---

## 步骤

| # | 工作项 | 前置 | 退出验证 |
|---|---|---|---|
| 1 | `robot-adapters/hipnuc` | 框架 IMU trait ready | io_thread + iceoryx2 `state/imu` 能出 500 Hz 状态 |
| 2 | `robot-adapters/damiao` | 框架 motor trait ready | 4 路 SocketCAN + MIT 模式，bench 上 1 电机 → 6 电机 → 23 电机逐步加 |
| 3 | `robot-policy-host` | 框架 policy_host crate ready | `ort` crate + warmup + 零拷贝绑定；ONNX dummy 模型 end-to-end |
| 4 | `robot-app-atom01` 骨架 | 上述 3 项 ready | config 解析 + obs builder 有 `--dry-run` dump 能力 |
| 5 | 训练侧 replay 对拍 | 步骤 4 `--dry-run` | 训练侧拉一条 replay，dry-run obs 与 replay buffer **比特一致**（见 [`atom01.md`](atom01.md) §2.11 不变量 #1） |
| 6 | 闭链解耦 Rust 版单元测试 | `close_chain.rs` 骨架 | 给定 `motor_q` → Rust 版 `joint_q` 与 C++ 原栈对拍 **1e-6** 级一致（[`atom01.md`](atom01.md) §2.8） |
| 7 | Bench 渐进测试 | 步骤 2 + 6 | 单电机 → 6 电机 → 23 电机逐步加负载，验证 RT 约束（p99、drop=0）|
| 8 | 真机小位移实验 | 步骤 7 | `kp` 减半、`clip_actions` 减半跑一个普通策略 |
| 9 | 全策略跑通 | 步骤 8 | normal / amp / attn_enc / beyondmimic / interrupt 五种模式都能跑且不抖 |
| 10 | 正式替换原 `atom01_deploy` | 步骤 9 稳定运行 ≥ 1 周 | 切换为默认部署栈；原栈保留作退路 |

---

## 退路策略

任何一步失败 → 原 `atom01_deploy` 仍在 `~/repo/robot/atom01_deploy` 可跑。

迁移是**可逆的 A/B**，不是一次性切换：
- `robot-system` 和 `atom01_deploy` 使用不同的 systemd unit 名；
- 切换通过 `systemctl disable atom01-deploy && systemctl enable robot-system-atom01` 完成；
- 回滚对称操作。

---

## 状态追踪

| # | 状态 | 备注 |
|---|---|---|
| 1 | ⏳ 未开始 | — |
| 2 | ⏳ 未开始 | — |
| 3 | ⏳ 未开始 | — |
| 4 | ⏳ 未开始 | — |
| 5 | ⏳ 未开始 | — |
| 6 | ⏳ 未开始 | — |
| 7 | ⏳ 未开始 | — |
| 8 | ⏳ 未开始 | — |
| 9 | ⏳ 未开始 | — |
| 10 | ⏳ 未开始 | — |

状态标签：⏳ 未开始 · 🟡 进行中 · ✅ 完成 · ⚠️ 阻塞。

---

**最后更新**：2026-04-24（从 `atom01.md` §8 拆出，明确标注为非 spec）。
