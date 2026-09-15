//! 保持同一user-bus原RefUnit，防止正常退出属性在取证前被GC；不改变unit政策。
//! 固定ABI依据：systemd v255 src/systemd/sd-bus.h；只加载下列七个稳定公开符号。

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use robot_core::time::{RobotClock, SystemClock};
use serde_json::{Value, json};

type Open = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type Ready = unsafe extern "C" fn(*mut c_void) -> c_int;
type Process = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type Wait = unsafe extern "C" fn(*mut c_void, u64) -> c_int;
type Timeout = unsafe extern "C" fn(*mut c_void, u64) -> c_int;
type Call = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
    *mut *mut c_void,
    *const c_char,
    ...
) -> c_int;

struct Library(*mut c_void);
impl Library {
    fn open() -> Result<Self, String> {
        // SAFETY: 常量NUL字符串和有效dlopen旗标；句柄仅由本对象dlclose一次。
        let pointer = unsafe {
            libc::dlopen(
                c"libsystemd.so.0".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if pointer.is_null() {
            Err("本地libsystemd.so.0不可加载，manager引用门不可用".into())
        } else {
            Ok(Self(pointer))
        }
    }
    fn symbol(&self, name: &CStr) -> Result<*mut c_void, String> {
        // SAFETY: 库仍存活，name是完整C字符串；后续只转为该公开符号的精确ABI。
        let pointer = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if pointer.is_null() {
            Err(format!("本地libsystemd缺符号{name:?}"))
        } else {
            Ok(pointer)
        }
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: 唯一库句柄，UnitReference先关闭原bus才销毁此字段。
        unsafe {
            libc::dlclose(self.0);
        }
    }
}

struct Api {
    open: Open,
    close: Close,
    ready: Ready,
    process: Process,
    wait: Wait,
    timeout: Timeout,
    call: Call,
}
impl Api {
    fn load(library: &Library) -> Result<Self, String> {
        // SAFETY: Linux POSIX dlsym的函数指针转换；每个名称/签名逐项对应原sd-bus.h。
        unsafe {
            Ok(Self {
                open: std::mem::transmute::<*mut c_void, Open>(
                    library.symbol(c"sd_bus_open_user")?,
                ),
                close: std::mem::transmute::<*mut c_void, Close>(
                    library.symbol(c"sd_bus_close_unref")?,
                ),
                ready: std::mem::transmute::<*mut c_void, Ready>(
                    library.symbol(c"sd_bus_is_ready")?,
                ),
                process: std::mem::transmute::<*mut c_void, Process>(
                    library.symbol(c"sd_bus_process")?,
                ),
                wait: std::mem::transmute::<*mut c_void, Wait>(library.symbol(c"sd_bus_wait")?),
                timeout: std::mem::transmute::<*mut c_void, Timeout>(
                    library.symbol(c"sd_bus_set_method_call_timeout")?,
                ),
                call: std::mem::transmute::<*mut c_void, Call>(
                    library.symbol(c"sd_bus_call_method")?,
                ),
            })
        }
    }
}

pub(super) struct UnitReference {
    bus: *mut c_void,
    api: Api,
    _library: Library,
    unit: CString,
    acquired: bool,
    connected: Value,
}

impl UnitReference {
    pub(super) fn connect(unit: &str, until: u64) -> Result<Self, String> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|e| e.to_string())?;
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").map_err(|e| e.to_string())?;
        // SAFETY: geteuid无参数/出参且不转移任何资源，仅读取本进程真实有效UID。
        let uid = unsafe { libc::geteuid() };
        validate_address(&runtime, &address, uid)?;
        if !unit.starts_with("r4c-")
            || !unit.ends_with(".service")
            || !unit
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err("只允许本包封闭unit引用".into());
        }
        let started = SystemClock.now().0;
        remaining_us(started, until)?;
        let library = Library::open()?;
        let api = Api::load(&library)?;
        let mut owner = Self {
            bus: ptr::null_mut(),
            api,
            _library: library,
            unit: CString::new(unit).map_err(|e| e.to_string())?,
            acquired: false,
            connected: Value::Null,
        };
        // SAFETY: 出参指向owner原bus槽；open_user只使用上面核过的本地Unix地址。
        let opened = unsafe { (owner.api.open)(&raw mut owner.bus) };
        code(opened, "sd_bus_open_user")?;
        if owner.bus.is_null() {
            return Err("原sd_bus_open_user成功但返回空指针".into());
        }
        // 不能让call_method内部bus_ensure_running借默认认证25s；先显式完成同一3s握手。
        loop {
            remaining_us(SystemClock.now().0, until)?;
            // SAFETY: 原bus非空且唯一拥有，未注册用户回调；这两接口不作无限等待。
            let ready = unsafe { (owner.api.ready)(owner.bus) };
            code(ready, "sd_bus_is_ready")?;
            if ready > 0 {
                remaining_us(SystemClock.now().0, until)?;
                break;
            }
            // SAFETY: 空message出参让库内部处理/回收原消息，不将C分配交给Rust。
            let progress = unsafe { (owner.api.process)(owner.bus, ptr::null_mut()) };
            code(progress, "sd_bus_process")?;
            let remaining = remaining_us(SystemClock.now().0, until)?;
            if progress == 0 {
                // SAFETY: 显式至多1ms且不越同一剩余预算；从不使用UINT64_MAX等待。
                code(
                    unsafe { (owner.api.wait)(owner.bus, remaining.min(1_000)) },
                    "sd_bus_wait",
                )?;
            }
        }
        owner.connected = json!({"address":address,"started_ns":started,"ready_ns":SystemClock.now().0,"deadline_ns":until,"open_result":opened});
        Ok(owner)
    }

    pub(super) fn acquire(&mut self, until: u64) -> Result<Value, String> {
        if self.acquired {
            return Err("禁止重复RefUnit".into());
        }
        let fact = self.call(c"RefUnit", until)?;
        Ok(json!({"connection":self.connected,"ref_unit":fact}))
    }

    pub(super) fn release(&mut self, until: u64) -> Result<Value, String> {
        if !self.acquired {
            return Ok(json!({"ref_unit_acquired":false}));
        }
        self.call(c"UnrefUnit", until)
    }

    fn call(&mut self, method: &CStr, until: u64) -> Result<Value, String> {
        let before = SystemClock.now().0;
        let timeout = remaining_us(before, until)?;
        // SAFETY: 握手已完成的同一原bus，设置当前调用剩余预算，不存在隐含认证等待。
        code(
            unsafe { (self.api.timeout)(self.bus, timeout) },
            "sd_bus_set_method_call_timeout",
        )?;
        // SAFETY: 固定Manager方法与"s"签名匹配唯一NUL结尾unit；错误/回复出参允许空。
        let result = unsafe {
            (self.api.call)(
                self.bus,
                c"org.freedesktop.systemd1".as_ptr(),
                c"/org/freedesktop/systemd1".as_ptr(),
                c"org.freedesktop.systemd1.Manager".as_ptr(),
                method.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                c"s".as_ptr(),
                self.unit.as_ptr(),
            )
        };
        if result >= 0 {
            self.acquired = method == c"RefUnit";
        }
        let observed = SystemClock.now().0;
        code(result, method.to_str().map_err(|e| e.to_string())?)?;
        remaining_us(observed, until)?;
        Ok(
            json!({"method":method.to_string_lossy(),"unit":self.unit.to_string_lossy(),"started_ns":before,"deadline_ns":until,"observed_ns":observed,"result":result}),
        )
    }
}

impl Drop for UnitReference {
    fn drop(&mut self) {
        if !self.bus.is_null() {
            // SAFETY: 唯一原bus使用配套close_unref；绝不flush，无无限poll或额外方法调用。
            unsafe {
                (self.api.close)(self.bus);
            }
            self.bus = ptr::null_mut();
        }
    }
}

fn code(result: c_int, operation: &str) -> Result<(), String> {
    if result < 0 {
        Err(format!(
            "原{operation}返回{result}：{}",
            std::io::Error::from_raw_os_error(-result)
        ))
    } else {
        Ok(())
    }
}
fn remaining_us(now: u64, until: u64) -> Result<u64, String> {
    until
        .checked_sub(now)
        .filter(|v| *v > 0)
        .map(|v| v.div_ceil(1_000))
        .ok_or_else(|| "原bus连接/调用成功观察也不得越过同一绝对截止".into())
}
fn validate_address(runtime: &str, address: &str, uid: libc::uid_t) -> Result<(), String> {
    if runtime != format!("/run/user/{uid}") || address != format!("unix:path={runtime}/bus") {
        Err("RefUnit只允许既定XDG_RUNTIME_DIR/bus单一本地Unix连接".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_bus_deadline_rejects_equal_and_late_success_without_reset() {
        assert_eq!(remaining_us(1_000, 2_000).unwrap(), 1);
        assert!(remaining_us(2_000, 2_000).is_err());
        assert!(remaining_us(2_001, 2_000).is_err());
    }
    #[test]
    fn unit_reference_rejects_nonlocal_multiple_or_foreign_user_bus_addresses() {
        assert!(validate_address("/run/user/1000", "unix:path=/run/user/1000/bus", 1000).is_ok());
        for address in [
            "tcp:host=localhost,port=1",
            "unix:abstract=other",
            "unix:path=/run/user/1001/bus",
            "unix:path=/run/user/1000/bus;tcp:host=localhost",
        ] {
            assert!(validate_address("/run/user/1000", address, 1000).is_err());
        }
        for runtime in [
            "/run/user/1001",
            "/run/user/1000;tcp:host=localhost",
            "/run/user/1000,guid=a",
            "/run/user/%31%30%30%30",
        ] {
            assert!(validate_address(runtime, &format!("unix:path={runtime}/bus"), 1000).is_err());
        }
    }
}
