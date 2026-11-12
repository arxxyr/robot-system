# systemd 部署

S0 当前状态与 roadmap 见 [S0 HAL 状态](../../docs/s0_hal_status.md)。RPC 网关属于上层接口并调用 S1，**不是 S0 的启动依赖或安装内容**。

## 当前入口

| 文件 | 职责 |
|---|---|
| `robot-s0-bus1.service` | 左臂、头部及头/左腕 RGB；guard 检查后 exec S0 |
| `robot-s0-bus2.service` | 右臂、底盘及右腕 RGB；guard 检查后 exec S0 |
| `robot-s0-preflight.service` | 启动前一次性整机停止交接：双总线17电机，轮子先零速、全部断扭并读回；不是常驻进程 |

依赖为 `preflight → guard → S0`。preflight 成功后保留 `active/exited`，同次开机内另一侧启动不重复执行；该完成态只证明上次检查结束，不证明此刻物理安全。重启主机不等于电机断电。

本机保留的 arm-only、S1 和 gateway 模板仍被兼容部署及测试调用，不能与 full S0 同时占用总线。三个 `*-freeze-test.service` 仅供隔离 smoke 渲染使用，不直接安装到实机。旧服务的停稳检查用于排除占用，不要求将其程序纳入 S0 产物清单。

## 安装与回滚

使用 [install_s0_units.py](../install_s0_units.py) 的 `check / backup / install / rollback`。先冻结已验 S0、guard、两份 full TOML、三个 unit、标定、相机 helper 和固定解释器，并核对 manifest SHA。按明确文件处理，不通配安装目录。

- `check`：核候选与外部文件、保护目录、当前服务和设备占用，不打开串口。
- `backup`：保存三旧、三新共六个路径的内容或缺失状态、权限及 SHA。
- `install`：同目录临时文件、fsync、原子写新三份，再删除已备份且未漂移的旧三份，随后仅 `daemon-reload`；不启动服务或设置自启。
- `rollback`：按同一备份恢复六个路径的原状；保留失败回执。仍被旧 unit 引用的程序、配置和标定必须保留。

四个操作都显式指定 `--candidate` 与 `--manifest-sha256`；写操作还须 root 和 `--backup-dir`。已有安装记录和当前备份位置只维护在当前状态中。改名前备份仍用其冻结旧安装器恢复；新安装器明确识别六路径备份格式。新 preflight 不继承旧 handoff 的完成态，首次运行须另确认现场条件。回滚保护范围不包括 latch、RX marker、锁或 guard 计数，不用旧快照解除保护。

## 开机自启

仍使用同一安装器及三个 unit，不增加服务或 S0 状态：

- `autostart-check`：只读核对已安装候选、设备占用、两份明确的 `auto_enable=false` 和双侧双份 EStop=1，报告自启状态及 RX 原值；不授权 RX。
- `backup`：在本批三个 unit 已安装且全部 disabled 时生成新的自启备份，沿用原六路径格式；安装前的旧备份不能替代它。
- `enable-autostart`：按新备份核对身份后，仅对固定三个服务执行 `systemctl enable`，不带 `--now`，不启动服务。
- `disable-autostart`：沿同份备份关闭三个服务的自启，可处理前次 enable 部分成功；撤销不要求运行产物完好或重启前的设备节点身份，但仍核对 unit、服务停止状态和保护文件。不停止进程、不修改保护文件；文件回滚仍须先停稳并关闭自启，再走原 `rollback`。

自启不代表控制授权。RX 一次性授权消费后的 Dirty 保持；没有有效授权时，即使启动停止读回成功，仍进入 `CommFault/BootUntrusted + AdapterUncertain`，不能宣称可信断扭。该路径不产生正常关节采样；现有故障退出也不能正常完成停止收尾。因此当前仅准备自启工具和备份，实际 enable 与重启暂缓；不得把故障待机或预期拒帧改成正常验收通过。

默认不上扭，故障及 EStop 不因启动而清除。不增加 S0 对外 Boot/Init 状态：进程初始化沿现有启动流程和 systemd `activating`；S1 已有 `Booting` 保留。后续真实开机验收须独立核对新 boot ID、服务状态、保护状态、心跳/RGB及原停止收尾，不能只检查 enabled 或 active。

## 正式启动验收

固定解释器、脚本、产物及 PLAN_ONLY 预检完成后，再确认本轮现场支撑和持续看护。按左、右顺序，上一侧停止与清场通过才启动下一侧；本轮保持 `auto_enable=false` 和 EStop 锁存，不发送 ClearEStop 或 SetEnabled。启动仍会访问 UART 并执行停止收敛。

正式运行由 systemd 管理，保留 `Type=notify`、watchdog、原重启策略及 journal。当前验收使用具名 `systemctl start/stop`，按 InvocationID 核对授权消费、正常 boot_ready、MainPID 与 cgroup；仅看到 READY 不够。观察各路状态与实际 RGB 数据的时间推进，退出后沿原维护入口做 STOP51、三轮稳定性复核及资源清场。

失败保留原件；不能自动重发一次性授权或重跑未知结果。停止读回未确认时，按既有维护入口要求进行现场处置。tmux 只承载临时编译和验证，不替代正式服务；自启配置与真实重启分别验收。D15 硬件 fail-safe 仍未闭环，`deployable=false`。

## 保护与恢复边界

| 对象 | 规则 |
|---|---|
| 总线与身份 | 同侧配置、robot/hardware ID、USB 和校准必须对应；full 候选 preflight 与两个 guard 使用同批配置 |
| 保护目录 | bus1 使用 `/var/lib/robot/xlerobot_left`，bus2 使用 `/var/lib/robot/xlerobot_right`；不得按新运行 ID 新建或迁移保护状态 |
| RX 信任 | 一次性授权只供本轮启动消费；消费后 Dirty 是正常结果。status/authorize/revoke 也持 RX 锁，不与同目录 daemon 并发，不自动续授 |
| EStop | latch 双份持久化；只通过 maintainer+ 的 ClearEStop 事务解除，成功后仍 Safe，上扭是另一个动作 |
| 实例锁 | 核真实持有者，待进程退出由 OS 释放；不删文件、换 inode 或凭锁内 PID 文字判断活进程 |
| 失败退出 | 86 为 guard 停止态，87 为 RX 锁竞争，89 为 latch 锁竞争；保留真实错误并修明根因，不清计数或 reset-failed 掩盖故障 |

`guard`、`preflight-*`、`provision` 有硬件或文件副作用，不能用于离线试运行。离线测试使用现有假步骤和临时目录；正式候选与服务产物目录不用于构建测试。
