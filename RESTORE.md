# 重装系统后恢复指南（robot-system）

> 重装后 `git clone` 完先读本文件。目标：恢复到"能继续候选 E Execute"的状态。
> 权威进度以 `team-swe/specs/v0_5_dimos_replace.md` + `git log` 为准，本文件是操作清单。

## 0. 远端仓库

| remote | 地址 | 说明 |
|--------|------|------|
| `origin` | `ssh://git@git.yang.cafe:20080/loosqk/robot-system.git` | 私有，主力，代码/文档/会话备份包都在这 |
| `github` | `git@github.com:arxxyr/robot-system.git` | 备用异地（可能落后，按需 `git push github master`） |

```bash
git clone ssh://git@git.yang.cafe:20080/loosqk/robot-system.git
cd robot-system
```

## 1. 恢复 Claude 会话 + memory（承载完整对话上下文）

仓库根有会话备份包 `claude-session-backup-<日期>.tar.gz`（会话 transcript +
项目 memory 快照；**这两样不在 chezmoi、不在别处，只在这个包里**）。

```bash
# 还原到 Claude 项目目录
mkdir -p ~/.claude/projects/-home-loosqk-repo-robot-robot-system
tar xzf "$(ls -t claude-session-backup-*.tar.gz | head -1)" \
    -C ~/.claude/projects/-home-loosqk-repo-robot-robot-system

# 在 robot-system 目录内接回完整对话（选 9fc2059c… 那个会话）
claude --resume
```

`--resume` 成功 = 完整上下文回来，直接从"段(4) 待 ACK、下一步段(5)"续。

**若不 resume（冷启接班）**：新 session 读这三处即可接上——
`CLAUDE.md` 状态块 / `team-swe/specs/v0_5_dimos_replace.md`（权威 Spec）/ `git log`。

## 2. 本地依赖仓库 / 文件（不在本仓 git，需重新准备）

| 路径 | 内容 | 恢复方式 |
|------|------|---------|
| `/home/loosqk/repo/robot/dimos-deploy` | XLeRobot 部署仓（移植参考源 + 候选 E 功能基线，**冻结 `9ece6bd`**，只读） | 重新 clone，checkout `9ece6bd` |
| `/home/loosqk/repo/robot/dimos` | 上游 dimos 框架（仅查上游时参考，非移植目标） | 重新 clone |
| `/home/loosqk/repo/dimensional-applications/calibration/` | 标定 JSON（权威 arm config 引用：`left_arm.json` / `right_arm.json` / `base.json` / `head.json`） | 从旧机备份或 **Orin `10.0.1.41` 上同名目录**取 |

## 3. 工具链 / 开发机

- **Rust nightly 浮动**（`rust-toolchain.toml` `channel = "nightly"`，不锁定；lint fix-forward，见 memory `nightly_float_no_pin`）。首次构建自动装当前 nightly。
- **Orin 开发机**：`ssh loosqk@10.0.1.41`，代理 `10.0.1.80:10808`（http/socks5）。真机 demo 前 `sudo systemctl stop xlerobot-coordinator xlerobot-web` 释放硬件。

## 4. 候选 E Execute 进度锚点（2026-07-17 保存点）

**Plan Approved — supervised-demo scope**（reviewer，Spec round 10）。执行严格串行：

```
(1)→(2)→(3)→(4)→(5)→(6)→(8)→(7)→(9)→(10-demo)
```

| 段 | 状态 | commit |
|----|------|--------|
| (1) layout 计算权威 `compute_layout_id` | ✅ push | `dca9a52` |
| (2) config typed motor_ids + 身份锁 + census + 权威 arm config ×2 | ✅ push | `539887f` / `2a08732` |
| (3) S0 layout 落地 + command gate | ✅ push | `737b555` / `548e05a` |
| (4) S1 layout + mock_s1 --config + robot-rpc client::layout 校验 | ✅ push，**⚠ 待 reviewer ACK** | `f51e5cc` / `1537252` / `799ce7b` |
| (5) G1 语义段（state cache + scripted_start_pose + 末点发布 + idle_hold_target） | ⬜ **待段(4) ACK 后开工** | — |

**接班硬约束**：段(4) 已 push 但 reviewer 尚未 ACK（为重装保存提前推的）。
**勿在段(4) round ACK 前进段(5)**。

## 5. 每段 gate（P.0 + PX-5，commit 前必过）

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p robot-tests --all-features --all-targets -- -D warnings   # feature 面永久 gate（PX-5）
cargo test --workspace   # 空载单跑（满载会触发既有 flake watchdog_withholds_ping，PX-4，段(8) 修）
# 再 release-first 重建 + 六 smoke（见 §6）
```

commit 拆分：feat/build 与 docs 分开提交，`Cargo.lock` 随 feat。

## 6. release + 六 smoke 调用（本轮踩坑复核，精确参数）

```bash
cargo build --release --workspace
cargo build --release -p robot-tests --features \
  "pubsub-cleanup-smoke,scripted-backend-smoke,ik-backend-smoke,robotctl-smoke,clear-estop-e2e"

./target/release/crash_restart_smoke                                    # 5/5
./target/release/crash_restart_smoke --use-mock-s1 --iterations 2       # 2/2（mock_s1 路径）
./target/release/scripted_backend_smoke --hardware-id smoke_arm --config configs/scripted_backend_smoke.toml --phase all   # 5/5
./target/release/ik_backend_smoke       --hardware-id ik_arm    --config configs/ik_backend_smoke.toml       --phase all   # 5/5
./target/release/pubsub_cleanup_smoke --bin-dir target/release --hardware-id smoke_arm --config configs/crash_restart_smoke.toml --phase all   # all + triggered_c3=false
./target/release/clear_estop_e2e --bin-dir target/release --case all    # case=all
./target/release/robotctl_smoke --config configs/robotctl_smoke.toml --hardware-id ik_arm --phase all   # 12/12

# freeze_systemd（真 systemd user）：先建 feature 版 s0，跑完还原干净 release s0
cargo build --release -p robot-s0 --features test-freeze-hook
./target/release/freeze_systemd_smoke --daemon-bin target/release/s0_daemon --iterations 1   # 1/1
cargo build --release -p robot-s0    # 还原干净 release
```
