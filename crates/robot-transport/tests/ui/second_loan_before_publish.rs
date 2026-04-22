//! 编译期负向测试：writer 持 `&'a mut Publisher` borrow；未 publish 就再
//! `loan()` 应编译失败（借用冲突）。
//!
//! 注意：NLL 允许"first borrow 在最后使用点释放"，所以必须让 w1 跨过 second
//! loan —— 在 second loan **之后**还用一次 w1，否则 NLL 会把 w1 的 borrow
//! 缩到 second loan 之前，test 就"通过编译"（和负向预期相反）。

use robot_msg::markers::JointState;
use robot_transport::mock::MockChannel;
use robot_transport::traits::Publisher;

fn main() {
    let ch = MockChannel::<JointState>::new();
    let mut publisher = ch.publisher();

    let mut w1 = publisher.loan().unwrap();
    // w1 仍持 &mut publisher borrow，下一行 loan 借用冲突：
    let _w2 = publisher.loan().unwrap();
    // 在 second loan 之后用 w1，阻止 NLL 把 borrow 缩短：
    w1.set_body_bytes(&[]);
}
