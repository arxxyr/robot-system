# 交接 · post-E 通信故障硬化

## 当前步骤

W3 生产总装正式 round4 已连续完成 31 门，全部退出 0；完整软件 All 场景
72/72。367 项源码和 228 项批准输入首末一致，独立源码与运行证据终审通过，root 已接受
APPROVED_SOFTWARE_SCOPE；本阶段提交见 git 日志。真实运行时间为
2026-09-10 02:10:15–02:22:51 UTC；不把此前开发轮拼成最终通过。

本包接通共享权限、高/普通双队列、唯一文件 writer 和 RPC 原责任账本。
每轮先处理安全降低，再处理恢复与普通工作；关节目标保留最新值。
超时、失联或业务失败不提前释放原 wire/队列责任；真实物理退休后才能回复。
通信不可信和三次断扭失败的软件负例保持责任至原 L 收尾，不能用超时冒充完成。

## 验收与证据入口

- 正式记录：H 包 `local/s48/w3/cc/round4/run.json`；来源清单
  `local/s48/w3/cc/source_files_final.sha256`，SHA `2872511c…cc01c5`。
- 源码覆盖：`local/s48/w3/cc/final_review_outputs/robot-w3-final-source-coverage-r3.json`。
  root 最终裁决见 `local/s48/w3/review.md`。
- 本地工作证据：`target/post_e_h2_rt/s48/w3/`；已合入唯一
  [H_evidence.tar.gz](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)，
  索引与范围在 [H.json](../../.verify/post_e_comm_fault_hardening/H.json)。
- round1–3 正式失败、开发失败、评审修正和真实 RED→GREEN 均保留。
  精简前交接原文保存在包内 `local/s48/w3/root_closeout/`；早期阶段以 H 子项、
  [验证记录](../../docs/10_verification.md) 与 git 提交为准，不在入口复制流水账。

当前统计分别为 workspace 2151/0/1、workspace doc 56/0/8、S0 特性 837/0/1
（通过/失败/忽略）；重叠分组不相加。禁分配正反例、dev/release 各 60 秒 soak、
真实 S1/网关进程及 transport 特性矩阵已执行。mock soak 的 strict_rt=false，
本轮没有 Orin、串口、电机或常驻服务操作。

## 分工与推进顺序

root 定方案、独立评审、文档与 git；具体实现/测试优先 cc-claude，额度耗尽后
按用户混编授权由原生 agent 继续。当前 A/B 分域开发并交叉评审，C 独占 Cargo、
rustfmt、构建和测试。最终评审由 root 或独立 Codex 完成；自查不代替终审。
共享文件明确唯一作者。每阶段评审和必要质量门通过即 commit，不 push。
不改冻结 `docs/00_design.md`，语义依据为 Spec S-48 与 PLAN P.6.14。

W3 基线为 W3a `757ac55`，本阶段提交以 git 日志为准；分支 `feat/post-e-comm-fault-hardening`。
W3 软件验收完成，接着按 W4 → P-E → 两端同源软件验证 → 新现场 H1–H3 顺序推进。
W4 文件崩溃、监督/收尾与双环境草案已有独立短审；具体故障夹具与函数写权
正在补齐，未批准实施或声称测试通过。RR 关闭标记的 Relaxed 与独立队列
Release/Acquire 之间仍欠完整可见性证明，单次追加 receive 的确定性交错测试
不替代弱内存证明；潜在漏回复可长期占用原credit，后续单列修正，不把
wire4 历史 FAIL 提升为 PASS。

P-E 保留原 20/s、固定 50ms 节拍、360–600 秒、成功唯一新写 >8192 与
实际 wire4 门；缓存阈值、背景 drain 及每种结果的 final_stop 不放宽。
不复用已消费的 `left_h2_current` 或旧现场驱动。

## 现场与未完成边界

左 H1/H3 PASS；H2 两次 FAIL，用户最后确认左臂断电、双臂有支撑。
第二次 H2 因 4096 缓存耗尽及背景启动迟到失败；后续 S-46 本地 8192 长测通过，
Orin 882 唯一写时真实 wire4 满 FAIL。失败原始证据及各自来源保留。
右臂未测；六服务最后已停止，D15 OPEN，deployable=false。本轮未重新查询远端，
这些是最后已知现场事实。新使能测试须先完成软件验收、提交、两端对齐，
再获取新的电源/USB 重置及持续在场事实。

Orin：`loosqk@10.0.1.40`，仓库同本机路径；调试或 RT 检查必要时直接 sudo。
标定沿用 Orin 上已核字段/ID 的 Python JSON。串口按 D2 by-id 身份选择，
不凭 ttyACM 序号；内核 CONFIG_PREEMPT=y，不称 PREEMPT_RT。
后续硬序保持通信故障硬化 → D15 退役决策 → robot-viz Rerun 桥 → F7 判据。
