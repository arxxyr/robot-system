
    #[test]
    fn review_nonfinite_scripted_position_is_accepted_and_completed() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let payload = build_scripted_payload_bytes(&[(vec![invalid, 0.0, 0.0], 10, 1)]);
            let (mut instance, cfg, hardware, anchor, clock) =
                fresh_instance_with_payload(payload, &[0.0, 0.0, 0.0], 1_000_000_000);
            let mut ctx = ActionContext {
                clock: &clock,
                layout_id: TEST_LAYOUT_ID,
                hardware: &hardware,
                config: &cfg,
                anchor_bias: &anchor,
                command_seq: 1,
                scripted_start_pose: None,
                solver: None,
            };
            for advance_ms in [0, 1] {
                clock.advance(robot_core::time::DurationNs::from_millis(advance_ms));
                let ActionTickOutput::Continue { command: Some(body), .. } =
                    instance.tick(&mut ctx).expect("实际后端没有拒绝非法目标")
                else { panic!("必须返回一帧非法目标"); };
                let command = root::<FbJointCommand<'_>>(body.as_bytes()).expect("真实命令编码通过结构验证");
                assert!(!command.target().expect("目标").get(0).is_finite());
            }
            clock.advance(robot_core::time::DurationNs::from_millis(10));
            assert!(matches!(instance.tick(&mut ctx).unwrap(),
                ActionTickOutput::Completed(ActionOutcome::Success)));
            assert!(!instance.successful_hold_target().expect("成功保持目标")[0].is_finite());
            println!("实际后端接受 {invalid}、发送非法目标、Completed(Success)、保留非法保持目标");
        }
    }
