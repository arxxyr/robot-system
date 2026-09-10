# 交接 · post-E 通信故障硬化

## 当前步骤

W4a文件同步故障夹具与两条真实软件恢复样例已独立源码/运行证据及root验收，
APPROVED_SOFTWARE_SCOPE；阶段提交见git日志，未push。基线W3已提交`1338f9d`。
正式round1连续15门满足预期（13个exit0、两个参数拒绝exit2），新增2/2及原All72/72。
373项来源与187项冻结输入首末一致，源码恰10改6新；fmt、三组Clippy零警告。
默认S0 605/0/0、完整hook850/0/1、工具79/0/0，重叠分组不相加；本轮未跑原60秒ignored soak。

两例分别在Prepare目录同步及首次Repair删除目录同步得到真实EBADF，沿原Latched
目标恢复，保首FsyncFailed、原RR唯一退休、marker无、双1/1/latchtrue；不重复unlink。
这是Linux同目录O_PATH故障输入，未称自然EIO、断电或电机验证。
两处客户端错误出口已修为原node/client/pending保留到原Child真reap。
原开发编译/Clippy失败，以及统计器旧模块前缀误写exit1均保留；后置r2只更正该
前缀并严格核9原名，独审后实际exit0。原plan/freeze/正式15门没有修改。

## 证据入口

- root裁决：`target/post_e_h2_rt/s48/w4a/review.md`。
- 正式run：`w4a/c/round1/run.json`，SHA `de0e9c63…71e608`；
  result为`w4a/c/result.json`，SHA `6bc84ff5…24e26c`。
- 最终源码清单：`w4a/c/formal_source.sha256`，373项，SHA `f34f4430…45e5e2`。
- B完整终审：`w4a/reviews/final/robot-w4a-final-source-evidence-independent-review.md`；
  A独立1358项身份核算与root原CSV核验同包保留，4份实际产物/796软件文件已核。
- 上述路径在唯一[H证据包](../../.verify/post_e_comm_fault_hardening/H_evidence.tar.gz)中
  对应`local/s48/`；精确索引、归档边界与SHA见[H.json](../../.verify/post_e_comm_fault_hardening/H.json)。
  W3及更早阶段记录以H子项、[验证记录](../../docs/10_verification.md)与git日志为准。

## 分工与下一步

root方案/终审/文档/git；cc-claude额度耗尽后按用户混编授权由原生A/C分文件开发，
C独占Cargo/rustfmt/构建与测试，B独立评审。每阶段完整质量门与评审通过即commit，
不push，不改冻结`docs/00_design.md`。所有新增实施先有精确函数包和独立方案审。

W3已将新权限、高/普通双队列、唯一writer、RPC责任账本接入生产调用方。
当前继续W4剩余：RR严格内核修正、逐步SIGKILL、监督/关闭与双环境；之后P-E。
RR r2及本方迁移表倾向直接替换现有工厂，保持topic/schema/FF，全部peer和自动
清理者须同批同源；仍需闭合最终shared析构顺序与公用CAL状态回归门，未批准vendor。
W4b固定5个SIGKILL/同目录重启候选正独审，未实施；完整T8/SUP/ENV仍未完成。
RR旧跨原子可见性与映射删除可能漏回复并长期占信用，不能说仅短延迟或已修复。
W1旧锁偶发失败仍列观察，本轮All通过不等于该历史问题的根因已闭合。

P-E保原20/s、50ms节拍、360–600秒、成功唯一新写>8192、真实wire4及所有结果final_stop。
完成本地包后再两端同源软件验证，最后新的现场H1–H3；不复用旧H2驱动或已消费授权。

## 现场边界

左H1/H3 PASS，H2两次FAIL；最后确认左臂断电、双臂支撑。S-46本地8192长测通过，
Orin真实882唯一写时wire4满FAIL；右臂未测，六服务最后停止，D15 OPEN、deployable=false。
本轮未访问远端，上述是最后已知事实。新使能前须取得新的电源/USB重置与持续在场事实。
Orin为`loosqk@10.0.1.40`，调试/RT需要权限直接sudo；标定沿Orin已核Python值，
串口按原D2 by-id身份。通信硬化后仍按D15退役决策→robot-viz Rerun桥→F7判据推进。
