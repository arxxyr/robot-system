//! 最新命令的固定值槽：非RT写端串行发布，RT读端只读预分配原子载荷。
//! RT必须使用固定一次的 `try_load`；失败保留上个完整命令及其原时间戳/期限。
//! 句柄的构造、派生与最终析构均在RT循环外，周期读不持Arc Guard或writer锁。

use std::sync::Mutex;

use crate::snapshot::{RtSnapshotPublisher, RtSnapshotReader, SnapshotPublishError, SnapshotValue};

/// 非RT命令发布失败；写端不再伪报成功，上一完整值保持可读。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandStoreError {
    #[error("命令发布失败：{0}")]
    Snapshot(#[from] SnapshotPublishError),
    #[error("命令写端锁已中毒")]
    WriterPoisoned,
}

/// 构造预分配的最新命令槽。
pub struct LatestCommandSlot<T: SnapshotValue>(std::marker::PhantomData<T>);

impl<T: SnapshotValue> LatestCommandSlot<T> {
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new(initial: T) -> (LatestCommandWriter<T>, LatestCommandReader<T>) {
        let (publisher, reader) = RtSnapshotPublisher::new(initial);
        Self::from_parts(initial, publisher, reader)
    }

    /// 仅供耗尽出口验收；生产组合不编译此入口。
    ///
    /// # Errors
    /// 版本必须是有效偶数，沿底层快照构造规则拒绝非法值。
    #[cfg(any(test, feature = "test-comm-fault-hook"))]
    pub fn new_with_version(
        initial: T,
        version: u64,
    ) -> Result<
        (LatestCommandWriter<T>, LatestCommandReader<T>),
        crate::snapshot::SnapshotVersionError,
    > {
        let (publisher, reader) = RtSnapshotPublisher::new_with_version(initial, version)?;
        Ok(Self::from_parts(initial, publisher, reader))
    }

    fn from_parts(
        initial: T,
        publisher: RtSnapshotPublisher<T>,
        reader: RtSnapshotReader<T>,
    ) -> (LatestCommandWriter<T>, LatestCommandReader<T>) {
        (
            LatestCommandWriter {
                publisher: Mutex::new(publisher),
                reader: reader.clone(),
                initial,
            },
            LatestCommandReader { reader, initial },
        )
    }
}

/// 单一非RT发布者；锁只保护写侧，不进入RT读取路径。
pub struct LatestCommandWriter<T: SnapshotValue> {
    publisher: Mutex<RtSnapshotPublisher<T>>,
    reader: RtSnapshotReader<T>,
    initial: T,
}

/// 可在初始化期派生的读句柄；周期读取只搬固定值。
pub struct LatestCommandReader<T: SnapshotValue> {
    reader: RtSnapshotReader<T>,
    initial: T,
}

impl<T: SnapshotValue> LatestCommandWriter<T> {
    /// 非RT发布；锁中毒与版本错误都必须由调用者处理。
    ///
    /// # Errors
    /// 写端锁中毒或底层固定值发布失败。
    #[inline]
    pub fn store(&self, value: T) -> Result<(), CommandStoreError> {
        self.publisher
            .lock()
            .map_err(|_| CommandStoreError::WriterPoisoned)?
            .publish(&value)
            .map_err(CommandStoreError::Snapshot)
    }

    /// 只在初始化期派生句柄；此调用会克隆底层Arc。
    #[must_use]
    pub fn reader(&self) -> LatestCommandReader<T> {
        LatestCommandReader {
            reader: self.reader.clone(),
            initial: self.initial,
        }
    }
}

impl<T: SnapshotValue> LatestCommandReader<T> {
    /// 构造槽时的完整初值，供RT建立本地缓存，不等待writer。
    #[must_use]
    pub const fn initial_value(&self) -> T {
        self.initial
    }

    /// RT只尝试一次；未读到一致值时不修改调用方缓存。
    #[inline]
    #[must_use]
    pub fn try_load(&self) -> Option<T> {
        self.reader.try_load()
    }

    /// 非RT读取；可能等待正在发布的writer，RT调用者必须使用 `try_load`。
    #[inline]
    #[must_use]
    pub fn load(&self) -> T {
        self.reader.load()
    }
}

impl<T: SnapshotValue> Clone for LatestCommandReader<T> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            initial: self.initial,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_and_latest_value_are_preserved_for_all_readers() {
        let (writer, reader) = LatestCommandSlot::new(42u64);
        let cloned = reader.clone();
        let derived = writer.reader();
        assert_eq!(reader.initial_value(), 42);
        writer.store(99).unwrap();
        for current in [&reader, &cloned, &derived] {
            assert_eq!(current.try_load(), Some(99));
            assert_eq!(current.load(), 99);
            assert_eq!(current.initial_value(), 42);
        }
    }

    #[test]
    fn writer_poison_is_reported_without_changing_the_previous_value() {
        let (writer, reader) = LatestCommandSlot::new(42u64);
        let _ = std::panic::catch_unwind(|| {
            let _lock = writer.publisher.lock().unwrap();
            panic!("只污染本测试写端锁");
        });
        assert_eq!(writer.store(99), Err(CommandStoreError::WriterPoisoned));
        assert_eq!(reader.try_load(), Some(42));
    }

    #[test]
    fn reader_and_writer_remain_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LatestCommandReader<u64>>();
        assert_send_sync::<LatestCommandWriter<u64>>();
    }

    #[test]
    fn exhausted_writer_reports_failure_and_keeps_the_previous_command() {
        let (writer, reader) =
            LatestCommandSlot::new_with_version(42u64, crate::snapshot::MAX_PUBLISHED_VERSION)
                .unwrap();
        assert_eq!(
            writer.store(99),
            Err(CommandStoreError::Snapshot(
                SnapshotPublishError::VersionExhausted
            ))
        );
        assert_eq!(reader.try_load(), Some(42));
    }
}
