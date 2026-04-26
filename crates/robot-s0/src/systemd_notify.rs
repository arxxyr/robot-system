//! systemd `sd_notify(3)` 协议轻量实现（**不**依赖 `sd-notify` 第三方 crate）。
//!
//! 协议：UNIX `SOCK_DGRAM`，messages 是 `KEY=VALUE\n` 多行文本，常用 key:
//! `READY=1` / `WATCHDOG=1` / `STOPPING=1` / `STATUS=<text>`。详
//! [`man:sd_notify(3)`](https://www.freedesktop.org/software/systemd/man/sd_notify.html)。
//!
//! ### 不在 systemd 下跑时的行为
//!
//! 当环境变量 `NOTIFY_SOCKET` **未设置**或为空字符串时，所有 notify 函数
//! 都 **no-op + 返回 Ok**。这样 `s0_daemon` 在裸命令行 / runbook 调试 /
//! crash_restart_smoke 等场景下都跑得通，跟 systemd 部署形态零差异。
//!
//! ### Abstract socket 限制
//!
//! 如果 `NOTIFY_SOCKET` 以 `@` 开头（Linux abstract namespace socket），本
//! 实现**当前不支持**，notify 会 silent skip。systemd 240+ 默认走 path-based
//! socket（`/run/systemd/notify`），实测 Ubuntu 22.04 / 24.04 / Tegra 全是
//! path-based，所以暂不阻塞。要支持 abstract 走 `libc::sendto` + 手撸
//! `sockaddr_un`（前置 `\0` 字节），可后续再加。
//!
//! ### Watchdog
//!
//! systemd 在 unit 用 `WatchdogSec=2s` 时把 `WATCHDOG_USEC=2000000` 注入
//! 环境。daemon 应每 `WATCHDOG_USEC / 2` 微秒调一次 `watchdog()`；不够及时
//! systemd 视为僵死，按 `Restart=` 策略重启。
//!
//! [`spawn_watchdog_thread`] 启动 dedicated 线程做这件事，传入 `stop` 原子
//! 让 daemon graceful shutdown 时停止 ping。

#![allow(clippy::doc_markdown)]

use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ============================================================================
// 一次性 notify
// ============================================================================

/// `READY=1` —— daemon 完成所有初始化（4 线程都 spawn + 进入主循环），systemd
/// 才认为 service "active"。`Type=notify` unit 必须收到 READY=1 才解锁
/// `After=` 链路上的下游 unit。
///
/// **必须**在所有 thread spawn + 初始 state 落地之后调一次。
pub fn ready() {
    let _ = send_message("READY=1\n");
}

/// `WATCHDOG=1` —— ping 一次。systemd 收到即重置 watchdog 计时器；
/// `WatchdogSec=2s` unit 不在 2s 内收到下次 ping 即按 `Restart=on-failure`
/// 处理。一般由 [`spawn_watchdog_thread`] 周期触发，不需要手调。
pub fn watchdog() {
    let _ = send_message("WATCHDOG=1\n");
}

/// `STOPPING=1` —— 通知 systemd 进入 graceful shutdown，给后续 SIGTERM /
/// timeout 流程一个明确"我在收尾"的语义。systemd 收到后切到 deactivating
/// 状态、停发 watchdog timeout 等。**只**在 daemon 主动退出时调一次。
pub fn stopping() {
    let _ = send_message("STOPPING=1\n");
}

/// `STATUS=<text>` —— 自由格式状态字符串，`systemctl status` 会显示。
/// 可用于"current state: Enabled.Active / Safe / Stopping" 这种摘要。
pub fn status(text: &str) {
    let payload = format!("STATUS={text}\n");
    let _ = send_message(&payload);
}

// ============================================================================
// watchdog 周期 ping 线程
// ============================================================================

/// 启动 dedicated watchdog ping 线程。读 `WATCHDOG_USEC` env 决定 ping
/// 间隔（= `usec / 2`），停在 `stop` 原子被 release 时。
///
/// **不在 systemd 下跑时**（`WATCHDOG_USEC` 未设置或 = 0）返回 `None`，
/// 调用方应当 graceful 处理（不需要 join）。
///
/// **不在 RT 线程内调**：内部 `UnixDatagram::send_to` 涉及 syscall + 内核
/// 队列写，违反 §0 / §4.9 RT 铁律。本线程是普通优先级 non-RT 工作线程。
#[must_use = "返回 Option<JoinHandle> 调用方应保留以便 graceful shutdown 时 join"]
pub fn spawn_watchdog_thread(stop: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if usec == 0 {
        return None;
    }
    // ping 间隔 = WATCHDOG_USEC / 2，给 systemd 留 50% 余量吸收 jitter。
    let interval = Duration::from_micros(usec / 2);
    let handle = thread::Builder::new()
        .name("s0_systemd_wd".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                watchdog();
                thread::sleep(interval);
            }
        })
        .expect("spawn s0_systemd_wd thread");
    Some(handle)
}

// ============================================================================
// 内部
// ============================================================================

/// 发送一条 notify 消息。`NOTIFY_SOCKET` 未设置 / abstract socket 时 no-op。
fn send_message(payload: &str) -> std::io::Result<()> {
    let socket_path = match std::env::var("NOTIFY_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()), // 不在 systemd 下：no-op
    };
    if socket_path.starts_with('@') {
        // Abstract namespace socket（Linux 扩展）。本实现暂不支持；systemd
        // 240+ 默认 path-based，主流环境无影响。详 module-level 文档。
        return Ok(());
    }
    let sock = UnixDatagram::unbound()?;
    sock.send_to(payload.as_bytes(), &socket_path).map(drop)
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    /// `NOTIFY_SOCKET` 未设置 → ready/watchdog/stopping/status 都 no-op + Ok。
    #[test]
    fn no_notify_socket_silent_noop() {
        // 保险：用 SAFETY 临时清掉这个 env 防止意外注入污染测试。
        // SAFETY: 在测试线程内同步操作 env，不与其它线程并发。生产代码不该这样做，
        // 但单测里只有 main + spawned test thread，可以接受。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
        // 这些都不应 panic 也不应 Err（目标语义：daemon 在 dev runbook 跑无副作用）。
        ready();
        watchdog();
        stopping();
        status("test");
    }

    /// 启动一个临时 UNIX datagram socket 模拟 systemd 接收端，验证
    /// `READY=1` payload 真的发出去了。
    #[test]
    fn ready_writes_payload_to_unix_datagram() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&sock_path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        // SAFETY: 同 no_notify_socket_silent_noop 注释。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }

        ready();

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv READY");
        assert_eq!(&buf[..n], b"READY=1\n");

        // 清掉避免后续 test 撞到
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    /// `STATUS=<text>` payload 拼接。
    #[test]
    fn status_payload_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&sock_path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        status("Enabled.Active (sub: Active, link: Ok)");
        let mut buf = [0u8; 256];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            "STATUS=Enabled.Active (sub: Active, link: Ok)\n"
        );
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    /// abstract socket 路径（@开头）silent skip，不报错。
    #[test]
    fn abstract_socket_silent_skip() {
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", "@/tmp/abstract.sock");
        }
        ready();
        watchdog();
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    /// `WATCHDOG_USEC` 未设置 → `spawn_watchdog_thread` 返 None。
    #[test]
    fn watchdog_thread_returns_none_without_usec() {
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_watchdog_thread(stop);
        assert!(h.is_none());
    }

    /// `WATCHDOG_USEC=0` → 返 None（避免 0 间隔 busy-loop）。
    #[test]
    fn watchdog_thread_returns_none_with_zero_usec() {
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "0");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_watchdog_thread(stop);
        assert!(h.is_none());
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
    }
}
