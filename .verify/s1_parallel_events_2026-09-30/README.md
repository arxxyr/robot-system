# S1 并行事件夹具候选

仅调整 `runtime.rs` 的 cfg(test) 夹具：独立 Node/端口 RAII、发送前刷新订阅连接，并用同 topic 双订阅者验证同一终态的投递与未连接遗漏。生产前缀 25621 字节一致，20ms/2s 预算和默认并行不变。

Mac 定向默认并行 15 项及 S1 lib Clippy 通过；隔离 Linux 更新浮动 nightly 5c543b0b8 后，fmt、根 Clippy all/all-targets 和完整 S1 lib 默认并行 287 项通过，0 failed/ignored。589 个输入前后一致，35 份 VM 命令回执真实 wait/reap。

入口：`summary.json`；逐文件基线比较：`source_comparison.json`；原生证据包：`linux-r2/native-evidence.tar.gz` 和对应 index；已校验解包：`linux-r2/native-extracted`。

首次 Linux 缺 rustfmt 的 FAIL 和 Mac 既有 all-targets handler lint FAIL 均保留。历史两次超时的具体根因仍 OPEN_UNCONFIRMED；本报告不宣称完整 workspace 测试或 H2 硬件通过。候选冻结，无主树源码修改、robot/Orin SSH、硬件访问、合并、提交或推送。
