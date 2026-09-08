# 2026-09-08 · Cargo 依赖升级

用户要求全部 Cargo 依赖更新至最新版本。本次覆盖 14 个 workspace 成员的 27 项外部直接依赖，保留既有 feature、可选依赖和精确 pin 策略。内部 crate 的统一版本仍为 0.1.2。

状态：最终锁文件已冻结，完整检查通过，独立评审 Approved，可提交。C5 功能验收仍在主工作树推进；本分包只基于 C4 提交 `437b14f` 验证依赖升级。

| 直接依赖 | 原锁定版本 | 本轮锁定版本 |
|---|---|---|
| libc | 0.2.185, 1.0.0-alpha.3 | 0.2.189, 1.0.0-alpha.4 |
| thiserror | 2.0.18 | 2.0.20 |
| flatbuffers | 25.12.19 | 25.12.19 |
| blake3 | 1.8.5 | 1.8.7 |
| iceoryx2 | 0.9.3 | 0.9.3 |
| arc-swap | 1.9.1 | 1.9.2 |
| tempfile | 3.27.0 | 3.27.0 |
| crossbeam-channel | 0.5.15 | 0.5.17 |
| hdrhistogram | 7.5.4 | 7.6.0 |
| serde | 1.0.228 | 1.0.229 |
| toml | 0.9.12+spec-1.1.0, 1.1.2+spec-1.1.0 | 0.9.12+spec-1.1.0, 1.1.5+spec-1.1.0 |
| serde_json | 1.0.149 | 1.0.151 |
| inventory | 0.3.24 | 0.3.24 |
| criterion | 0.8.2 | 0.8.2 |
| serialport | 4.9.0 | 4.10.0 |
| assert_no_alloc | 1.1.2 | 1.1.2 |
| trybuild | 1.0.117 | 1.0.121 |
| tracing | 0.1.44 | 0.1.44 |
| tracing-subscriber | 0.3.23 | 0.3.23 |
| ctor | 1.0.8 | 1.0.13 |
| tonic | 0.14.6 | 0.14.6 |
| tonic-prost | 0.14.6 | 0.14.6 |
| prost | 0.14.4 | 0.14.4 |
| uuid | 1.23.1 | 1.26.0 |
| tonic-prost-build | 0.14.6 | 0.14.6 |
| protoc-bin-vendored | 3.2.0 | 3.2.0 |
| tokio | 1.52.1 | 1.53.1 |

`libc` 延续项目已批准的 1.0 预发布轨道，精确锁定 alpha.4。`flatbuffers` 与 vendor flatc 同为 25.12.19；`iceoryx2` 精确锁定 0.9.3。其它依赖使用当前非撤回正式版本；传递依赖遵守其上游版本约束，未强行跨主版本覆盖。

最终 `Cargo.lock` SHA256：`15adc9f22a87609b410b79208a096a82d62c7bc4ad8af614a8ca7bf845a41232`。

锁文件共有 262 项 registry package，108 个包名的版本集合发生更新、增减或收敛。
独立评审逐项核对直接依赖的当前发布轨道、registry checksum 与撤回状态，没有
撤回版本；传递依赖保留其上游约束内的合法重复版本。

首轮检查发现宽版本要求的锁定结果落后于最终注册表解析；旧锁的检查记录不计入最终结果。重新执行 `cargo update`，之后全部验证使用 `--locked`，并以文件内容和注册表逐项复核。

完整验收修正两处旧夹具 / 工具兼容问题：13 电机握手响应改为真实 ID；trybuild
仅在独立子进程允许生成清单的 unused workspace dependency 警告，其余警告仍拒绝，
三份负向快照没有更新。cargo-deny 同时补登记原有 assert_no_alloc / serialport
许可证，范围限定为各自精确版本，详 [CHANGELOG](../CHANGELOG.md)。

验证结果与具体命令见 [机器可读证据](../../.verify/dependency_upgrade_2026-09-08/report.json)。

- 提交前 fresh fmt、默认与全 feature Clippy 均零警告通过。
- 全 feature lib / bins 1461 PASS；完整 tests 1643 PASS、2 项既有 ignored。
- trybuild 三份编译失败快照严格通过；父环境强制彩色输出时仍通过子进程无色过滤。
- build 全 targets / features、cargo-deny 四门、schema_lint 均通过。
- 构建耗时见 [Cargo timing 报告](../../.verify/dependency_upgrade_2026-09-08/cargo-timing.html)。

完整 tests 显式使用本分包实际构建的 S0 / S1 / gateway，包括 8 项启动验收和
45 项 RPC 进程回归；产物路径、SHA256、toolchain 与完整日志随证据保存。
上述测试数存在包含关系，不相加。两项 ignored 是 rt_guard soak 和真 S1 启动
诊断，归 C5 的独立执行门，本分包不把它们计为通过。

本分包不部署到 Orin，不改变 D15 OPEN，也不替代 H1/H2/H3 的受监督现场测量。
