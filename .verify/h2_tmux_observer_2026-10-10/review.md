# 软件观察候选独审与 root 核验

独审为 proc_retry_review；两次只读审查未访问机器人、修改文件或重跑测试。root 阅读实际候选、质量入口和机制原件，并核主树/native 597项逐文件 SHA/bytes/executable 相同。

候选 SHA79653898…与原生r2相同，保留exec、一次launch和原run终态门。实际tmux3.2a六类无害场景PASS，9个r2 observer自然wait后二次waitpid ECHILD；r1另1个observer及原FAIL保留。mock driver为tmux自有子进程，仅据pane_dead/status，不称测试父进程Popen reap。已有局部hook/独立channel保持；当前继承的pane-died为空。未注入真实SSH断线、服务器重启或真实H2。

机制原件独算自然32 trial/501次读=313 observed+188 unreadable；188为161 R→R、27 R→Z，同PID/start/boot/PGID/UID绑定已知实验child。另controlled1 child两次读及一次同步/WNOWAIT独立记录，不计自然样本。33 child退出0、无信号、wait后ECHILD10；后验owner和33PID缺失仅证明自有实验后验，不称整机清场。三导出Python与当前主树一致；native unrelated a96与export352准确区分，旧585588归属仍UNKNOWN。

质量原包d36baf8e…含入口及13逐child launch/completion/stdout/stderr，哈希/字节匹配；严格串行、exit0/reapedtrue、signals空、session残余空。fmt结束3356100745596701后Clippy于3356100765965216启动；根all/all-targets -D warnings0。32dba69d69c5b10ea89a4042de8d0619f7756203与Cargo首尾一致，native a96 clean、597前后及主树逐项相同。共享Runner仅绑定d6b20da2…SHA，原包不含其正文；本轮依据既有Runner回执核回收，未重审该Runner实现。root本机实际fmt通过；不称Mac根Clippy通过。

上述核验只支持软件候选，不关闭硬件FAIL、STOP34 NOT_RUN或D15。新版32d六配方/七件及后续mock/tonic原进程均已完成，独审及root软件验收通过，范围如下。

新原生包8d420169…/425017B/254文件独审PASS，root复核原包字节/45回执SHA、exit0/reap、四份源清单和post身份前后相同。独审逐项核33+12个唯一直接child的PID/argv/身份/串行顺序、日志SHA/bytes、signals和会话残余；55个内部path/SHA引用相符。a96首尾clean、352真实ancestor命令0、61变更只.verify/HANDOFF；三helper原git-show输出与主树/源绑定，四处rustc为32d。六recipe Cargo JSON及七sidecar/manifest相符，七输出fresh=false；生产无测量hook、测量特性正确。七help/default PLAN_ONLY0，原生mock4×2为8软件样本、物理UART=false，不是Orin P99验收。

tonic原stdout六方法全通过、九JSON场景与scene存档一致，server_joined=true/active_requests=0，已绑定生产测试源在输出前await server join，负控保留。七冻结件/两manifest/正式四件身份前后相同；六服务四窗inactive/dead/PID0，四进程snapshot clean。普通拒读由同代次root observed完整身份解析为sd-pam/sshd，不放过未知进程。外层owner.log尾JSON与result相符、owner.exit0、pane终态有记录；仅称tmux/壳退出证据，不称测试父Popen wait。

限制：原包无七ELF本体、共享Runner/post_release_checks本体，只核SHA绑定、原回执和Cargo JSON。release33 child回执为V2六变量集合，代理/CARGO_HOME只由实际入口脚本及顶层字段绑定；post12 child已逐项展示这些override。fmt/Clippy与本轮quality r2交叉绑定，124Python是历史a30引用，本轮未重跑。f92草稿未执行，唯一实际5f5稿只增加shown环境展示，两稿均保留。新硬件/GO、完整Rustworkspace/新RT硬件门未做，旧H2 FAIL/STOP34 NOT_RUN、D15及deployable=false保持。
