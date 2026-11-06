
    #[test]
    fn review_all_position_nan_writes_center() {
        use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
        let cal = MotorCalibration::new(1, 0, 0, 1000, 3000).unwrap();
        let (mut adapter, handle) = make_adapter_with_calibration(&[("j1", 1)], vec![cal]);
        let mut command = JointCommandBuf {
            joint_count: 1,
            mode: ControlModeRepr::Position,
            selection: JointSelection::All,
            ..JointCommandBuf::default()
        };
        command.positions[0] = f32::NAN;
        adapter.write_command(&command.as_view()).expect("现有实现接受了NaN");
        let bytes = handle.drain_tx();
        assert_eq!(bytes, [0xff, 0xff, 0xfe, 0x07, 0x83, 0x2a, 0x02, 0x01, 0xff, 0x07, 0x44]);
        println!("现存缺陷已复现：NaN被写成ID1的Goal_Position=2047，内存串口帧={bytes:02x?}");
    }
