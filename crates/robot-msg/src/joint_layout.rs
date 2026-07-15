//! joint layout id 计算权威（v0.5 候选 E 段(1)，Spec D1.1）。
//!
//! `JointState.layout_id` / `JointCommand.layout_id` 的**唯一**取值来源；
//! S0 / S1 / client 各消费端一律经本函数从 config 派生，禁止各自再实现。
//!
//! ## 编码（长度前缀规范编码）
//!
//! ```text
//! enc = lp("robot-layout-v1")            —— domain/version 常量
//!     ‖ lp(robot_id)
//!     ‖ lp(hardware_id)
//!     ‖ u32_le(joint_count)
//!     ‖ ∀i ∈ config 顺序: lp(name_i) ‖ u32_le(motor_id_i)
//! 其中 lp(x) = u32_le(len(x)) ‖ x
//!
//! layout_id = u64::from_le_bytes(blake3(enc).as_bytes()[0..8])   （端序显式 LE）
//! ```
//!
//! 三性质（Spec D1.1）：
//! - **规范编码字段边界无歧义**：长度前缀消除跨字段拼接歧义
//!   （`("ab","c")` ≠ `("a","bc")`；64-bit 截断 hash 数学上非单射，
//!   本性质只宣称编码层字段边界）；
//! - **域隔离**：domain 常量把本编码与其他 blake3 用途隔离；
//! - **可演进**：v1 → v2 换 domain 常量走 `docs/CHANGELOG.md`（golden 快照
//!   必然打破，防静默漂移）。
//!
//! **config 顺序敏感**：覆盖 `joint_names` + `motor_ids` + order 三成分
//! ——`docs/00_design.md` §3.5.1 字面 "sorted_…+order" 的可执行化解读，
//! 解读差异随段(3) 登记 `docs/CHANGELOG.md`，`00_design.md` 0 改。

/// D1.1 domain/version 常量。改动 = 编码 v2，必须走 `docs/CHANGELOG.md`
/// （`golden_vector` 快照会打破，防静默变更）。
const LAYOUT_DOMAIN_V1: &[u8] = b"robot-layout-v1";

/// 从身份 + 关节表（config 顺序的 `(joint_name, motor_id)` 对）计算
/// `layout_id`。编码见模块 doc；确定性、对 name/id/order 敏感。
#[must_use]
pub fn compute_layout_id(robot_id: &str, hardware_id: &str, joints: &[(&str, u32)]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    lp(&mut hasher, LAYOUT_DOMAIN_V1);
    lp(&mut hasher, robot_id.as_bytes());
    lp(&mut hasher, hardware_id.as_bytes());
    hasher.update(&u32_len(joints.len()).to_le_bytes());
    for (name, motor_id) in joints {
        lp(&mut hasher, name.as_bytes());
        hasher.update(&motor_id.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut first8 = [0_u8; 8];
    first8.copy_from_slice(&digest.as_bytes()[0..8]);
    u64::from_le_bytes(first8)
}

/// `lp(x) = u32_le(len(x)) ‖ x` 喂进 hasher。
fn lp(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&u32_len(bytes.len()).to_le_bytes());
    hasher.update(bytes);
}

/// 长度收窄到 u32 域（规范编码依赖精确长度；超域 = 输入异常，fail-fast）。
fn u32_len(len: usize) -> u32 {
    u32::try_from(len).expect("layout 编码字段长度超 u32 域")
}

#[cfg(test)]
mod tests {
    use super::compute_layout_id;

    /// D2 权威左臂身份（`configs/xlerobot_left_arm.toml` 同源字面）。
    const LEFT_ARM_JOINTS: &[(&str, u32)] = &[
        ("left_arm_shoulder_pan", 1),
        ("left_arm_shoulder_lift", 2),
        ("left_arm_elbow_flex", 3),
        ("left_arm_wrist_flex", 4),
        ("left_arm_wrist_roll", 5),
        ("left_arm_gripper", 6),
    ];

    /// `schema_lint` fixture 首条（`schema_lint.rs` check #2 同字面输入）——
    /// 与 bin 侧 `lint_same_source_golden` 断言**同一**常量：任一侧
    /// （lib 编码 / lint fixture）漂移都会红。
    const LINT_FIXTURE_JOINTS: &[(&str, u32)] = &[
        ("left_shoulder_pan", 1),
        ("left_shoulder_lift", 2),
        ("left_elbow_flex", 3),
        ("left_wrist_flex", 4),
        ("left_wrist_roll", 5),
        ("right_shoulder_pan", 6),
        ("right_shoulder_lift", 7),
        ("right_elbow_flex", 8),
        ("right_wrist_flex", 9),
        ("right_wrist_roll", 10),
    ];

    #[test]
    fn deterministic() {
        let a = compute_layout_id("xlerobot_left", "left_arm", LEFT_ARM_JOINTS);
        let b = compute_layout_id("xlerobot_left", "left_arm", LEFT_ARM_JOINTS);
        assert_eq!(a, b);
    }

    #[test]
    fn order_sensitive() {
        let mut swapped: Vec<(&str, u32)> = LEFT_ARM_JOINTS.to_vec();
        swapped.swap(0, 1);
        assert_ne!(
            compute_layout_id("xlerobot_left", "left_arm", LEFT_ARM_JOINTS),
            compute_layout_id("xlerobot_left", "left_arm", &swapped),
        );
    }

    #[test]
    fn id_sensitive() {
        let mut re_id: Vec<(&str, u32)> = LEFT_ARM_JOINTS.to_vec();
        re_id[5].1 = 60;
        assert_ne!(
            compute_layout_id("xlerobot_left", "left_arm", LEFT_ARM_JOINTS),
            compute_layout_id("xlerobot_left", "left_arm", &re_id),
        );
    }

    #[test]
    fn name_boundary_injective() {
        // 长度前缀字段边界无歧义：无前缀时 "ab"+"c" 与 "a"+"bc" 拼接同串。
        assert_ne!(
            compute_layout_id("r", "h", &[("ab", 1), ("c", 2)]),
            compute_layout_id("r", "h", &[("a", 1), ("bc", 2)]),
        );
    }

    /// 固定输入 → 固定 u64 快照，锁编码稳定性（AC-E1）：domain/version、
    /// lp 结构、端序任一变更必然打破本断言。
    #[test]
    fn golden_vector() {
        assert_eq!(
            compute_layout_id("xlerobot_left", "left_arm", LEFT_ARM_JOINTS),
            0x2ee9_193f_6726_2ed2,
        );
    }

    /// lint 路径与直调同值：本断言与 `schema_lint.rs` bin 测
    /// `lint_same_source_golden` 锁**同一**常量（输入 = lint fixture 首条）。
    #[test]
    fn lint_same_source() {
        assert_eq!(
            compute_layout_id("xlerobot_01", "bus1_arms", LINT_FIXTURE_JOINTS),
            0x73d3_5e2d_536a_7374,
        );
    }
}
