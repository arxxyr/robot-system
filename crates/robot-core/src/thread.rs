//! 线程调度 / 锁内存 / CPU 亲和性 的 Linux syscall 薄封装。
//!
//! `docs/00_design.md` §4.2 把 `robot-core` 对 `libc` 的依赖限在 `time` +
//! "未来 `os` 抽象模块" 一类；本文件就是这个 "os 抽象" 的首个成员，承载
//! `s0_rt_thread` 启动时必须的三件事（§1.3 S0 RT thread 启动流程）：
//!
//! 1. `set_sched_fifo(prio)` —— `SCHED_FIFO` + `sched_priority = prio`
//! 2. `mlockall()` —— `MCL_CURRENT | MCL_FUTURE`，避免 RT 线程触发 page fault
//! 3. `pin_to_cpu(cpu)` —— `pthread_setaffinity_np` 绑核（Part 1 可选，Part 2 逼近 baseline 门槛必选）
//!
//! ### 平台
//!
//! 本模块 **仅 Linux**。`cfg(target_os = "linux")` 之外的 target 给出明确的
//! `unimplemented!` 路径 —— 其他 OS（macOS / Windows）不应进 S0 RT 闭环。
//!
//! ### 权限
//!
//! - `SCHED_FIFO`：需要 `rtprio` ulimit > 0（非 root 用户要 `/etc/security/limits.d/*.conf`
//!   配合），否则 `sched_setscheduler` 返回 `EPERM`。
//! - `mlockall`：需要 `memlock` ulimit 足够大，否则 `ENOMEM` / `EPERM`。
//! - `pthread_setaffinity_np`：普通用户可用（在被允许的 CPU 集合上）。
//!
//! 调用方按业务策略决定遇 `EPERM` 是 fail 还是 warn-and-continue。本模块只负责
//! 把 syscall 结果原样返回为 `io::Error`，不做降级。
//!
//! ### `libc` 使用声明
//!
//! 本模块与 `time.rs` 并列，属 `docs/00_design.md` §4.2 允许列表的例外，登记见
//! `docs/CHANGELOG.md` 2026-04-21 条目（覆盖 `time` + 未来 `os` 模块）。

#![cfg(target_os = "linux")]

use std::io;

/// 把当前线程设置为 `SCHED_FIFO`，优先级 `prio`（典型 70）。
///
/// 失败原因（`io::Error`）：
/// - `EPERM`：ulimit rtprio 不足或 `CAP_SYS_NICE` 缺失
/// - `EINVAL`：`prio` 超出内核允许范围（`sched_get_priority_min/max(SCHED_FIFO)`）
///
/// # Errors
///
/// 当 `sched_setscheduler(2)` 失败时返回 `io::Error::last_os_error()`。
pub fn set_sched_fifo(prio: i32) -> io::Result<()> {
    // SAFETY: libc 调用，param 为栈上固定大小 struct；0 表示当前线程（即当前调用线程）
    //        在 Linux 下对应 `gettid()` 的含义（per sched_setscheduler(2)）。
    let param = libc::sched_param {
        sched_priority: prio,
    };
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &raw const param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `mlockall(MCL_CURRENT | MCL_FUTURE)` —— 锁定当前 + 后续所有页，防 RT 线程触发
/// page fault。
///
/// # Errors
///
/// 当 `mlockall(2)` 失败时返回 `io::Error::last_os_error()`。典型失败：`EPERM` 或
/// `ENOMEM`（memlock ulimit 太小）。
pub fn mlockall() -> io::Result<()> {
    // SAFETY: libc 调用，参数为 const int flags，无内存别名问题。
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 把当前线程 pin 到指定 CPU（逻辑 CPU 编号）。
///
/// # Errors
///
/// 当 `cpu` 超出 `cpu_set_t` 容量（1024 bits），或 `pthread_setaffinity_np(3)`
/// 返回非零，均回传 `io::Error`。
pub fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    // libc::cpu_set_t 容量：aarch64/x86_64 Linux 下 1024 bits（CPU_SETSIZE = 1024）。
    if cpu >= cpu_set_size_bits() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cpu index out of cpu_set_t capacity",
        ));
    }
    // SAFETY: cpu_set_t 是 POD，zeroed 合法；CPU_SET 写入指定 bit（使用者保证 cpu < capacity）。
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: pthread_self 在当前线程上下文中合法；&set 指向栈上有效 cpu_set_t。
    let rc = unsafe {
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &raw const set,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // pthread_* 家族返回 errno 值，不走 errno；构造 io::Error::from_raw_os_error(rc)。
        Err(io::Error::from_raw_os_error(rc))
    }
}

/// `cpu_set_t` 可表示的最大 CPU 索引上界（典型 1024）。常量供测试引用。
#[must_use]
pub const fn cpu_set_size_bits() -> usize {
    // CPU_SETSIZE = sizeof(cpu_set_t) * 8；libc 未导出常量，这里按 layout 推算。
    std::mem::size_of::<libc::cpu_set_t>() * 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// 判断 `io::Error` 是否为 `EPERM`（操作不允许，通常是权限缺失）。
    fn is_eperm(e: &io::Error) -> bool {
        e.raw_os_error() == Some(libc::EPERM)
    }

    #[test]
    fn set_sched_fifo_ok_or_eperm() {
        // 在 CI / 普通 dev 机上大概率 EPERM（未配 rtprio）；Orin 上配完 limits.d 后会 Ok。
        // 两者都接受，防止 CI 不稳定。
        match set_sched_fifo(1) {
            Ok(()) => {
                // 成功设置后恢复默认（CFS / SCHED_OTHER）避免后续测试被影响。
                let param = libc::sched_param { sched_priority: 0 };
                let _ = unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &raw const param) };
            }
            Err(e) => assert!(is_eperm(&e), "unexpected error: {e}"),
        }
    }

    #[test]
    fn mlockall_ok_or_enomem_or_eperm() {
        match mlockall() {
            Ok(()) => {
                // 清理：munlockall 让测试进程退出时不影响其他测试。
                let _ = unsafe { libc::munlockall() };
            }
            Err(e) => {
                let code = e.raw_os_error();
                assert!(
                    code == Some(libc::EPERM) || code == Some(libc::ENOMEM),
                    "unexpected mlockall error: {e}"
                );
            }
        }
    }

    #[test]
    fn pin_to_cpu_works_on_cpu_0() {
        // cpu 0 在任何系统上都存在；普通用户通常可以 pin 到 cpu 0。
        match pin_to_cpu(0) {
            Ok(()) => {}
            Err(e) => {
                // 容忍极端情况（被 cgroup 排除 CPU 0 等）。
                assert_ne!(e.kind(), ErrorKind::InvalidInput);
            }
        }
    }

    #[test]
    fn pin_to_cpu_rejects_out_of_range() {
        let over = cpu_set_size_bits() + 1;
        let e = pin_to_cpu(over).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidInput);
    }
}
