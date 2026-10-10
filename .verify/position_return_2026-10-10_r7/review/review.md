爸爸，15项只读采集原件独审通过。完整TX/RX的ID、地址、宽度、长度、校验、error=0与原payload均与实际stdout一致；327个直接child/654输出哈希吻合且自然回收，251项exit0，76项fuser空输出的预期exit1，不能写为全exit0。driver624034/%56实际终态exit0。

| 电机 | 最小启动扭矩24 raw | 积分限制25 raw | 角度分辨率30 raw | Torque_Enable | Present_Velocity raw |
|---|---:|---:|---:|---:|---:|
| 左5 | 16 | 0 | 1 | 0 | 0 |
| 左7 | 16 | 0 | 1 | 0 | 0 |
| 左8 | 16 | 0 | 1 | 0 | 0 |

厂家表181名义：addr24单位0.1%，16仅按此表算术为1.6%；addr25为raw×4，0关闭积分限制功能，不是关闭积分控制；addr30为传感器最小分辨角的放大系数。现场FW主/次本轮未读，不能用文档version181保证适用性，不能据本表认定回程偏差原因或建议参数写入。

只读事实不构成17电机完整停止验证或往返回程/底盘验收；旧±50原因仍OPEN。未启用、未写参数、未执行动作、无自动重试或人工断电要求。逐项原帧、command_sequence与原始时间见review.json。

两次本地独审解读断言错误已逐字保留：最后18条是原finally非UART后验cleanup=true；plan顶层script_sha256是继承d97维护源，候选完整selfref在plan.script=189bb/17224。原run/plan/tar均未改，不计为采集或硬件失败。
