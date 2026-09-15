# 交接 · post-E 通信故障硬化

## 当前步骤

R4c 监督、原责任与本地 manager 的 36 项修改已通过源码评审、正式 round3 全部 51 门、
独立物证终审及 root 验收。564 项全仓输入首末/当前一致；8 件正式产物、77 个测试 ELF
均已独立逐 SHA 核验。26 份原报告全部通过，239 次调用/237 个唯一组合，新矩阵 19 个
profile 实例；F1 是原 writer 真 join/文件回调集成，不与外部 daemon 场景混算。
验收入口为 H 包 local/s48/rr_r4c/root_acceptance.md、final_binding.json、
final_evidence_review.md、gates/round3/run.json；完整源码表 final_source_round3.sha256。
构建基线 24b2cf3 加精确 36 项修改；阶段提交以 git 日志及包外 commit_receipt.json 为准。
原 round1 第 33 门 NoFrame 失败、round2 第 45 门 manager 身份采样失败均保留。
修复分别是原 1.1 秒内唯一缺帧重试、同代 READY 后原截止内一次严格身份核验；
没有改业务预算。round2 未保存坏环境，具体原因不冒实证。manager 引用只防 unit GC，
S12 原两秒代际捕获不变。开发失败与正式成功不拼接；当前包 post 与提交回执下包携带。
主开发已停写归还 Cargo；root 归档提交，不 push。默认并行 CI 锁例仍未修，另有只读诊断。
后续按已批准 R4d-A 原连接压力 → R4d-B 双主机同源 → P-E 长负载推进。
用户要求继续有效开发直到周额度剩 20%；最近实查剩 32%，接近线不派新大包，达到线真实交接。
本包仅本地 mock/user manager，无 SSH/电机访问；D15 OPEN、deployable=false。
旧 Orin 硬件状态与物理来源身份保持，尚未运行本轮 R4/ARM 验收。

W4b固定五例SIGKILL/原目录重启已通过独立源码、运行证据及root验收，
APPROVED_SOFTWARE_SCOPE；基线W4a为`382ca44`，本包提交见git日志。
W4b已提交`a2598a5`。用户2026-09-14恢复持续推进，并确认cc-claude额度重置；
RR R0已由cc-claude完成，独立Codex及root验收通过；root负责提交，不push。
两个0.9.3原发布包接入唯一path图，14个本方成员及其余锁版本不变；唯一源码差异为
`fetch_update`改名`try_update`，两个Relaxed及原闭包不变。round2的fmt、三Clippy、
transport、RPC与build七门全0；transport99/0/3、RPC304/0/0，既有ignored保留。
同图tree/deny引用round1实际成功；原三个编译失败与三个nonmember探针101保留。
证据入口H包`local/s48/rr_r0/`，root裁决`root_acceptance.md`、独审`independent_review.md`。
R0已提交`cbed745`。R1 CAL转移/真实enqueue/同源模型已由cc-claude实现、独立Codex与
root验收并提交`0513a8c`；R2原生开发已完成：源码独审ACK，正式round1共26门全部通过，已获root本地验收并提交06fcf0b，精确身份见git日志及commit_receipt。
R2最终交付为`target/post_e_h2_rt/s48/rr_r2/handoff.md`及`result.json`；43项输入首末一致，
IOX默认42/0/1、故障45/0/1、CAL45/0/0、模型6/0/0、workspace2207/0/9、S0 hook656/0/0。
R3已直接迁移本方既有工厂与生产调用方，不增第二套公开入口；本地验证不代替Orin。
R1正式round1共21门全0，CAL45、IOX库内8、模型6、workspace2207/0/9、S0 hook656/0/0。
库内测试真实经过Clippy、CAL no_std通过；根14成员保持，两vendor运行时闭包80包身份
与嵌套图一致，模型依赖不进生产图。测试三项发现均已修，开发失败保留。
证据在H包`local/s48/rr_r1/`；入口`root_acceptance.md`、`result.json`、两个独立评审。
正常finish与清理ORPHANED分开；同值CLOSED仍须R2实际route/pin和未复用资格才能证明
原请求完成。本包不宣称完整RR或Orin通过，归档边界以H.json为准。

## 已提交W4b证据

正式round1连续16门全部满足原期望（14个exit0、两个参数拒绝exit2）。
376源/129事前输入首末一致，恰8改3新，无依赖/构建图或冻结设计变更。
默认S0 605/0/0、完整hook858/0/1、工具89/0/0；重叠分组不相加，原60秒soak仍ignored。
新增SIGKILL5/5、原W4a2/2、原All72/72；4实际产物、979软件文件已逐件核验。

五例均原Child真signal9/reap，旧RR端点随后才结束；首Clear没有回复，不声称已退休。
重启复用原持久目录与锁，首轮14文件完整集合/SHA不变。空marker拒启动；
合法marker的1/0与0/0保守恢复1/1；无marker的0/0原加载false。四次正常重启的
初始latch与只读回复均直接验证，未使能硬件。同内核SIGKILL不等于掉电验证。
原compile/scope/software/Clippy失败共11轮36raw保留；首五软件FAIL未回填为PASS。

## 证据入口

- root裁决：`local/s48/w4b/review.md`。
- 正式run：`w4b/c/round1/run.json`，SHA `a2ea5c48…6ad7d`；
  result为`w4b/c/result.json`，SHA `bc1588d4…64911`。
- 最终376源清单：`w4b/c/formal_source.sha256`，SHA `7b65294c…cb674`。
- B非作者终审：`w4b/reviews/final/robot-w4b-final-source-evidence-independent-review.md`。
  B原件389项、A身份二路5073项、root原件119项均无失败。
- 上述w4b路径在唯一[H证据包](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)中
  对应`local/s48/`；精确索引、SHA及归档边界见[H.json](../../.verify/post_e_comm_fault_hardening/H.json)。
  本次归档后审查与commit回执留本地w4b/post_archive_review及commit_receipt.json；
  不声称位于此前生成的tar内。W4a归档后回执和RR只读方案已随本包携带。

## 后续路线

用户最新要求开发改用原生子agent。CC在2026-09-14额度限制处停止，root已停止其背景agent并退出会话（PTY70557 exit0）；R2剩余及后续由原生开发子agent实现/自测、另一个独立子agent评审，root方案/整合/提交。
开发子agent仍可按需并行拆分，明确写入范围，Cargo继续由一个开发者串行拥有。

W3已将新权限、高/普通双队列、唯一writer、RPC责任账本接入生产调用方。
W4a文件同步故障、W4b固定五例及RR R0–R3均已本地验收。W4剩余为R4
完整T8/SUP/ENV和双环境；随后P-E。每包必要检查与root评审通过即commit。
RR r3析构与公共CAL两项方案已ACK，直接替换现有工厂，保topic/schema/FF；
所有peer与自动清理者同批同源，模型/迁移实现按分包开发和评审。
旧跨原子可见性与映射删除问题已在R1–R3实现并通过对应本地验收，仍待R4双环境验证，不能外推为完整候选闭环。
W1旧锁偶发失败仍列观察，本轮All通过不等于其根因已闭合。
P-E保原20/s、50ms节拍、360–600秒、成功唯一新写>8192、真实wire4与所有结果final_stop。

## 现场边界

左H1/H3 PASS，H2两次FAIL；最后确认左臂断电、双臂支撑。S-46本地8192长测通过，
Orin真实882唯一写时wire4满FAIL；右臂未测，六服务最后停止，D15 OPEN、deployable=false。
上述硬件状态沿用最后现场记录；R4准备仅只读盘点远端软件环境与源码残留。
新使能前须取得新的电源/USB重置与持续在场事实，
不复用旧H2驱动或已消费授权。Orin为`loosqk@10.0.1.40`，调试/RT必要时直接sudo；
标定沿Orin已核Python值，串口按原D2 by-id身份。候选后续顺序保持D15→Rerun桥→F7。
