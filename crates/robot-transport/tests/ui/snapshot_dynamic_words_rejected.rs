//! 编译期负向测试（S48 A3b）：`SnapshotValue::Words` 只能是固定长度 `[u64; N]`；
//! 动态集合（`Vec<u64>`）不满足 sealed 的 `SnapshotWords`，实现契约必须编译失败。

use robot_transport::SnapshotValue;

#[derive(Clone, Copy)]
struct Dynamic(u64);

impl SnapshotValue for Dynamic {
    type Words = Vec<u64>;

    fn encode(&self, words: &mut Self::Words) {
        words.push(self.0);
    }

    fn decode(words: &Self::Words) -> Self {
        Self(words[0])
    }
}

fn main() {}
