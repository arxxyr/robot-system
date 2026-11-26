  <!-- XLeRobot 0.2.0 (Vector-Wangel/XLeRobot@850355e2)，simulation/xlerobot.xml。
       单轮取自 simulation/assets/simplify_4-Omni-Directional-Wheel_Single_Body-v1.stl，
       已简化三角面供三处共用。许可证见 meshes/omni-wheel.LICENSE。
       将原装配的单轮方向转至车头 +X，保留原轮心高度和轴向；以下固定关节仅用于显示。 -->
  <material name="drive_wheel_tire">
    <color rgba="0.28 0.29 0.30 1"/>
  </material>

  <link name="drive_wheel_front">
    <visual>
      <geometry><mesh filename="meshes/omni-wheel.stl" scale="0.001 0.001 0.001"/></geometry>
      <material name="drive_wheel_tire"/>
    </visual>
  </link>
  <joint name="drive_wheel_front_mount" type="fixed">
    <parent link="base_link"/>
    <child link="drive_wheel_front"/>
    <origin xyz="0.128250 0 0.051360" rpy="-1.570796 -0.261799 1.570796"/>
  </joint>

  <link name="drive_wheel_left">
    <visual>
      <geometry><mesh filename="meshes/omni-wheel.stl" scale="0.001 0.001 0.001"/></geometry>
      <material name="drive_wheel_tire"/>
    </visual>
  </link>
  <joint name="drive_wheel_left_mount" type="fixed">
    <parent link="base_link"/>
    <child link="drive_wheel_left"/>
    <origin xyz="-0.064525 0.111761 0.051360" rpy="1.570796 -1.832596 0.523599"/>
  </joint>

  <link name="drive_wheel_right">
    <visual>
      <geometry><mesh filename="meshes/omni-wheel.stl" scale="0.001 0.001 0.001"/></geometry>
      <material name="drive_wheel_tire"/>
    </visual>
  </link>
  <joint name="drive_wheel_right_mount" type="fixed">
    <parent link="base_link"/>
    <child link="drive_wheel_right"/>
    <origin xyz="-0.064125 -0.111068 0.051360" rpy="-1.570796 1.308997 -0.523599"/>
  </joint>
