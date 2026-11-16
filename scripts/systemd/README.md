# systemd 部署

S0 当前状态与 roadmap 见 [S0 HAL 状态](../../docs/s0_hal_status.md)。RPC 网关属于上层：任务调用S1，状态及使能/急停接口接S0，**不是 S0 的启动依赖或安装内容**。

## 当前入口

| 文件 | 职责 |
|---|---|
| `robot-s0-bus1.service` | 左臂、头部及头/左腕 RGB；guard 检查后 exec S0 |
| `robot-s0-bus2.service` | 右臂、底盘及右腕 RGB；guard 检查后 exec S0 |
| `robot-s0-preflight.service` | 启动前一次性整机停止交接：双总线17电机，轮子先零速、全部断扭并读回；不是常驻进程 |

依赖为 `preflight → guard → S0`。preflight 成功后保留 `active/exited`，同次开机内另一侧启动不重复执行；该完成态只证明上次检查结束，不证明此刻物理安全。重启主机不等于电机断电。

原arm-only S0/S1/gateway模板仍被兼容部署及测试调用，不能与full S0同时占用总线。三个 `*-freeze-test.service` 仅供隔离 smoke 渲染使用，不直接安装到实机。旧服务的停稳检查用于排除占用，不要求将其程序纳入 S0 产物清单。

`robot-s1-bus1.service`是左总线S1模板，硬依赖`robot-s0-bus1.service`，复用bus1的8关节布局和既有scripted profile；普通线程限制在CPU0–3，沿原5秒watchdog。左侧控制候选保留已安装S0设备配置，合入当前仓库bus1的`[s1.profile]`与已有运动片段，同一进程处理多次任务，不修改S0配置。模板设`Restart=no`，异常退出后须先确认断扭，再人工恢复，不能自动重建发布者或靠重启清错。它不访问串口、不自动使能或解除急停；启动S1可能沿依赖启动左S0，仍须事先通知并确认现场。常驻控制的运行状态见[当前状态](../../docs/s0_hal_status.md)，不纳入S0包；须通过下述安装器冻结候选，不能直接复制模板覆盖环境。控制前提与实施顺序见[通讯方案](../../docs/control_plan.md)。

## 只读状态接入模板

`robot-gateway-bus1.service`、`robot-gateway-bus2.service`对齐两份full配置与身份，分别绑定本机50061/50062；`robot-status.service`独立运行只读HTTP/WebSocket页面。三份均只设置`After`，无`Requires/Wants`，不会拉起S0或S1；S0不反向依赖它们，无首帧时页面显示无反馈。

这些模板不在S0安装器、发布矩阵或自启清单内。状态部署入口会冻结产物并渲染绝对路径，不直接复制模板覆盖当前正式安装。原网关保留原生控制RPC；默认只读候选使用[rpc-roles-status.toml](../../configs/rpc-roles-status.toml)的空凭据表与viewer权限，不授予控制能力，也不覆盖原角色表。只读模式不发送控制请求；显式左侧控制模式见下文。当前构建方式见[WebSocket入口](../../docs/control_plan.md#当前websocket入口)；浏览器不设置token，外部接入管理由用户已有系统承担。

状态页模板的初始资源预算为`CPUAffinity=0-3`、`CPUQuota=50%`、`MemoryHigh=128M`、`MemoryMax=256M`、`TasksMax=32`；CPU预算合计为半个逻辑核，任务数统计OS进程/线程而非Tokio任务。语义见[systemd资源控制](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)。退出最多等待5秒，60秒内最多启动5次。只限制独立Web服务，不修改S0/S1服务；当前部署已核对实际限额，结果见[当前状态](../../docs/s0_hal_status.md)，不据此推断未来相机预览负载峰值。

### 状态服务安装与回滚

使用[install_status_units.py](../install_status_units.py)，复用原文件身份校验、备份及同目录原子替换。每份候选只管理固定三个unit：

| prepare模式 | 管理的unit | 接入范围 |
|---|---|---|
| 默认只读 | `robot-gateway-bus1`、`robot-gateway-bus2`、`robot-status` | 两源只读状态与相机预览 |
| `--left-control` | `robot-s1-bus1`、`robot-gateway-bus1`、`robot-status` | 左侧显式使能及单关节定位；Web仍展示右侧，只读右网关不更改或重启 |

安装器不检查串口占用、不触碰保护文件，不带任何start/stop/enable操作；S0可保持运行。左侧模式须在S1已断扭、停止后安装或回滚，不能把停止S1等同于断扭。

- `prepare --candidate <绝对目录> --bin-dir <隔离构建的release目录> --bus1-config <当前S0配置绝对路径> --bus2-config <当前S0配置绝对路径> --runtime-commit <实际构建提交的完整SHA> --listen <机器人私网IP>:<Web端口>`：在仓库脚本位置执行，复制所需AArch64程序，渲染配置和三个unit，冻结文件及外部两份S0配置身份。输出清单SHA；候选目录必须不存在。准备仅处理文件，不操作服务。
- 左侧控制在同一prepare命令追加`--left-control`，**必须以模板service User执行，不用root代替**；产物目录还须包含`robot-s1`。生成的`xlerobot-bus1-s1.toml`保留外部S0配置，使用当前仓库`configs/xlerobot-bus1.toml`的`[s1.profile]`并追加原运动片段；旧S0配置无需自带S1后端，Web仅开放bus1，左网关保持本机50061。内部角色文件`rpc-roles-web-control.toml`及`web-control.env`由prepare生成，权限0600且归service User所有，不进Git或浏览器；仅授予现有状态、任务、使能、急停及显式解除急停权限，不自动清错。
- `check --candidate <目录> --manifest-sha256 <SHA>`：只读核候选和本批三个服务，拒绝运行中服务、待执行Job、未知drop-in或路径漂移，不要求停止S0。模式从已绑定manifest读取，后续操作无需再次传`--left-control`。
- `backup/install/rollback --candidate <目录> --manifest-sha256 <SHA> --backup-dir <绝对目录>`：写操作使用root。先备份三份unit原内容、权限或缺失状态，再安装；安装和回滚只处理本批unit并执行`daemon-reload`。失败保留进度回执；回滚允许已知的部分安装，拒绝未知漂移。回滚前须停稳本批服务；左侧模式不操作右网关、S0或保护文件，候选内部凭据仍保持原服务用户及0600权限。

开发自测用`uv run --no-project python`，实机执行沿已核对的固定Python。本轮验证与开放状态统一见[TODO8.3](../../docs/s0_hal_status.md#通讯与控制todo)。先完成候选、脚本和预检，再确认现场并通知启动；默认只读模式依次启动两网关和Web，左侧模式按原计划启动左S1、左网关和Web，右侧服务保持原状。启动验收核两源身份与8/9槽位、进程复用和原资源限额；动作验收再沿原位对齐、显式使能、滑条松手单次提交及±20raw到位合同，停止检查不放宽。S1异常后不自动重启、清错或使能，须确认断扭后人工恢复。失败只回收本轮拥有资源，未知结果不重试；通过后是否保持运行按本轮授权，自启另行安排。

Nginx由用户在其他配置中维护：上游使用安装时配置的机器人私网地址和Web端口，原样转发`/`、`/api/state`、`/api/ws`，支持WebSocket升级；具体要求见[通讯方案](../../docs/control_plan.md#当前websocket入口)。本项目不安装或重载Nginx。项目不记录具体外网URL，验收结果见[当前状态](../../docs/s0_hal_status.md#通讯与控制todo)；本机HTTP、外网WSS与有效设备反馈分别验收。`Type=simple`的start成功不等于HTTP已监听，验收需在有限预算内等待就绪，再开始观察；不能因首次连接拒绝立即判定程序启动失败。

## 安装与回滚

使用 [install_s0_units.py](../install_s0_units.py) 的 `check / backup / install / rollback`。先冻结已验 S0、guard、两份 full TOML、三个 unit、标定、相机 helper 和固定解释器，并核对 manifest SHA。按明确文件处理，不通配安装目录。

- `check`：核候选与外部文件、保护目录、当前服务和设备占用，不打开串口。
- `backup`：保存三旧、三新共六个路径的内容或缺失状态、权限及 SHA。
- `install`：同目录临时文件、fsync、原子写新三份，再删除已备份且未漂移的旧三份，随后仅 `daemon-reload`；不启动服务或设置自启。
- `rollback`：按同一备份恢复六个路径的原状；保留失败回执。文件回滚不要求串口或相机在线，仍检查服务PID/Job、备份和目标文件及保护文件不变，不启动服务。仍被旧 unit 引用的程序、配置和标定必须保留。

四个操作都显式指定 `--candidate` 与 `--manifest-sha256`；写操作还须 root 和 `--backup-dir`。已有安装记录和当前备份位置只维护在当前状态中。改名前备份仍用其冻结旧安装器恢复；新安装器明确识别六路径备份格式。新 preflight 不继承旧 handoff 的完成态，首次运行须另确认现场条件。回滚保护范围不包括 latch、RX marker、锁或 guard 计数，不用旧快照解除保护。

升级已安装的同名三 unit 时，`check / backup / install` 须成对指定 `--previous-candidate` 与 `--previous-manifest-sha256`，当前三份必须完整匹配上一版，且保护域及六个保护文件路径与新候选一致。备份记录上一版清单和三份源文件身份，安装须沿用同一选择并再次核对；旧设备枚举和运行 ELF 不作为升级依赖，新候选仍完整检查。`rollback` 只从本批备份还原原件，自启操作和回滚均不接受上一版参数。

## 开机自启

仍使用同一安装器及三个 unit，不增加服务或 S0 状态：

- `autostart-check`：只读核对已安装候选、设备占用、两份明确的 `auto_enable=false` 和双侧双份 EStop=1，报告自启状态及 RX 原值；不授权 RX。
- `backup`：在本批三个 unit 已安装且全部 disabled 时生成新的自启备份，沿用原六路径格式；安装前的旧备份不能替代它。
- `enable-autostart`：按新备份核对身份后，仅对固定三个服务执行 `systemctl enable`，不带 `--now`，不启动服务。
- `disable-autostart`：沿同份备份关闭三个服务的自启，可处理前次 enable 部分成功；撤销不要求运行产物完好或重启前的设备节点身份，但仍核对 unit、服务停止状态和保护文件。不停止进程、不修改保护文件；普通文件回滚须先停稳并关闭自启，同名升级备份的回滚则沿原记录保留自启状态。

自启不代表控制授权。两侧正式模板显式传入 `--resume-rx-on-clean-exit`：只有本轮已有可信RX授权、没有故障、原完整关闭及断扭确认通过、全部设备线程已退出，才将下一次RX授权双写为 `0`；下一次启动仍消费它并重新预检，保持 `auto_enable=false` 和原EStop。可信的Disabled/EStop待机沿既有路径持续读位置，不发送动作目标。该选项默认关闭，维护入口的原Dirty收尾合同不变。

当前已有Dirty、通讯故障或关闭失败不能靠此选项自动恢复；首次仍需原维护流程的一次RX授权。没有有效授权时，即使启动停止读回成功，仍进入 `CommFault/BootUntrusted + AdapterUncertain`，不产生正常关节采样。停止时保留原有界退出码1及未完成责任证据，再独立执行原STOP51确认物理停止。运行中崩溃、尚未交接时仍留Dirty；授权提交之后进程被终止不保证回到Dirty。不能删marker、用反复重启或把故障待机改判为healthy Safe来绕过。

续授权提交后的存储错误会尽力撤回为Dirty；若撤回也失败，保留授权状态未确认，不能仅凭退出码1认定文件为Dirty。此时仍沿原保护恢复入口处理，不自动重试。

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
| RX 信任 | 启动消费一次性授权后写 Dirty；status/authorize/revoke 持 RX 锁，不与同目录 daemon 并发。默认不续授；正式模板仅在上述可信干净退出条件下交接下一次授权 |
| EStop | latch 双份持久化；只通过 maintainer+ 的 ClearEStop 事务解除，成功后仍 Safe，上扭是另一个动作 |
| 实例锁 | 核真实持有者，待进程退出由 OS 释放；不删文件、换 inode 或凭锁内 PID 文字判断活进程 |
| 失败退出 | 86 为 guard 停止态，87 为 RX 锁竞争，89 为 latch 锁竞争；保留真实错误并修明根因，不清计数或 reset-failed 掩盖故障 |

`guard`、`preflight-*`、`provision` 有硬件或文件副作用，不能用于离线试运行。离线测试使用现有假步骤和临时目录；正式候选与服务产物目录不用于构建测试。
