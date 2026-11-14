# 按需开始

先读项目 [AGENTS.md](../AGENTS.md)，再按任务选择下面的入口及相关章节。

| 本次任务 | 入口 |
|---|---|
| 当前完成边界、阻塞和 roadmap | [S0 HAL 状态](s0_hal_status.md) |
| 对外接口、管理页面与单关节控制 | [通讯与控制方案](control_plan.md)：共享状态与控制链路、默认只读及部署边界；顺序见当前状态的TODO |
| 实现或核对架构契约 | [冻结设计](00_design.md) 的相关章节；冲突以此为准 |
| 模块、目标平台与规范 | [文档导航](README.md) |
| 验证合同与停止保护 | [验证约定](10_verification.md)；测试结果与原帧留在本地或Orin，不提交Git |
| 发布、安装与回滚 | [配置矩阵](../configs/package-matrix.toml)、[systemd维护说明](../scripts/systemd/README.md) |

先确认工作区，再搜索具体符号或任务；保留已有修改。

```bash
git status --short --branch
rg -n '本次任务的符号或关键词' crates configs scripts docs
```

Rust使用当前nightly；构建和测试复用同一个隔离产物目录，不能覆盖服务引用的产物。变更按受影响范围验证，提交前依次执行：

```bash
cargo fmt --all
export CARGO_TARGET_DIR="$PWD/target/verify-current"
cargo clippy --all --all-targets -- -D warnings
```

纯文档核对内容与链接，并执行上述提交检查；不重跑功能测试或硬件流程。源码、软件验证、部署和实机验收分别记录；旧 GO 不证明本次现场就绪。

开发提交只同步dev。正式发布沿现有打包脚本和矩阵，先确定最终提交再构建，核对包内提交、master与新SemVer标签相同；旧标签保留。发布包不自动安装，实机启动仍走既有预检与现场确认。

## 在 htop 中查看进程

Linux 控制与硬件测试入口统一使用 `robot-` 短进程名前缀：`robot-s0`、`robot-s1`、`robot-gateway`、`robot-ctl`、`robot-deploy`、`robot-feetech`、`robot-maint`、`robot-h2` 等；开发测试入口也采用该前缀。名称在启动时设置，不改变 Cargo 二进制名、服务名或控制接口。

htop 的 F2 → Display options 中启用 **Show merged command**（`show_merged_command=1`），然后按 F4 输入 `robot-`；也可运行 `htop -F robot-`。需要只看进程时按 H 隐藏用户线程；同一PID下的线程不是多个控制实例。服务已停止时不会显示运行PID，`enabled`只表示已配置开机启动。

当前维护入口给冻结 Feetech 工具使用临时 `robot-feetech` 符号链接作为启动名称，原文件及 SHA 校验保持；日志同时记录实际 `argv` 和原 `requested_argv`，子进程回收后删除链接。显示名不能作为安全身份或批量终止依据。

<a id="rerun-观测工具当前开发入口"></a>

## 只读观测

已有 `robot-viz` 订阅 S0 的观测镜像，可保存状态与 RGB：

```bash
robot-viz --config <本次实际使用的配置.toml> \
  --save <输出.rrd> --duration-ms 20000 --max-frames 10000 \
  --report <报告.json>
```

它不启动 S0/S1、不访问串口、不发控制命令；必须已有匹配配置的发布者。核对 RRD 中实际收到的各路状态、像素及时间推进，topic 存在不等于收到数据。实机启动沿[部署入口](../scripts/systemd/README.md)，观测结果不代替现场动作与停止确认。
