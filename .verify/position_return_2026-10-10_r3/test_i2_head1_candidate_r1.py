"""爸爸，固定head1(left7) I2候选的最小内存对照；不执行工具或UART。"""
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest

spec = importlib.util.spec_from_file_location("i_trial_candidate", Path(__file__).with_name("i2_trial_head1_candidate_r1.py"))
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)


class CandidateCases(unittest.TestCase):
    def setUp(self):
        self.values = dict(c.BACKUP)
        # 当前动态位置与旧备份不同仍允许；不追旧GP0/PP2642。
        self.values.update(Goal_Position=2648, Present_Position=2648)
        self.events = []
        self.io = SimpleNamespace(tool={"path": "position-tool"}, command=lambda argv, **kw: self.events.append(("command", argv)))
        self.engine = SimpleNamespace(io=self.io, rows=[], manual_cut=False, check_gate=lambda: self.events.append(("gate",)),
                                      plan={"position_delta_raw": 24, "return_tolerance_raw": 4, "position_wait_ms": 800})

    def read(self, _m, _engine, register, cleanup=False):
        self.events.append(("read", register, cleanup))
        return {"raw_unsigned": self.values[register]}

    def write(self, _m, _engine, value, cleanup=False):
        self.events.append(("write", value, cleanup))
        self.values["I_Coefficient"] = value

    def original(self, engine, side, motor, name, calibration):
        self.events.append(("original_position",))
        self.assertEqual(engine.plan, {"position_delta_raw": 24, "return_tolerance_raw": 4, "position_wait_ms": 800})
        self.assertEqual((side, motor, name), c.TARGET)
        engine.rows.append({"status": "PASS", "cleanup_errors": [], "position_accuracy": {"status": "PASS"}})

    def run_candidate(self, original=None, writer=None):
        return c.run_trial(None, self.engine, *c.TARGET, {}, original or self.original,
                           reader=self.read, writer=writer or self.write)

    def test_happy_restore_keeps_original_gates_and_fresh_dynamic_values(self):
        self.run_candidate()
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False), ("write", 0, True)])
        self.assertEqual(self.engine.rows[0]["i_trial"]["restore_status"], "PASS")

    def test_i2_ack_uncertain_still_restores_without_motion(self):
        def uncertain(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 2:
                raise ValueError("I2可能已生效但ACK失败")
        with self.assertRaisesRegex(ValueError, "ACK失败"):
            self.run_candidate(writer=uncertain)
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertNotIn(("original_position",), self.events)
        self.assertEqual(self.engine.rows[0]["i_trial"]["restore_status"], "PASS")
        self.assertEqual(self.engine.rows[0]["status"], "FAIL")

    def test_accuracy_fail_stays_fail_after_confirmed_restore(self):
        def accuracy(engine, *args):
            self.original(engine, *args)
            engine.rows[-1]["status"] = "FAIL"
            engine.rows[-1]["position_accuracy"]["status"] = "FAIL"
            raise ValueError("POSITION_ACCURACY_TIMEOUT")
        with self.assertRaisesRegex(ValueError, "POSITION_ACCURACY_TIMEOUT"):
            self.run_candidate(original=accuracy)
        self.assertEqual(self.values["I_Coefficient"], 0)
        self.assertEqual(self.engine.rows[0]["position_accuracy"]["status"], "FAIL")
        self.assertFalse(self.engine.manual_cut)

    def test_original_stop_uncertain_blocks_restore_and_future_uart(self):
        def stop_unknown(engine, *args):
            self.original(engine, *args)
            engine.manual_cut = True
            raise ValueError("原safe_off失败")
        with self.assertRaisesRegex(ValueError, "safe_off失败"):
            self.run_candidate(original=stop_unknown)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False)])
        self.assertFalse(any(e[0] == "read" and e[-1] is True for e in self.events))
        for tool in ("position-tool", c.RAW_TOOL["path"]):
            with self.assertRaisesRegex(ValueError, "粘性拒绝"):
                self.io.command([tool, "read"], cleanup=True)
        self.io.command(["systemctl", "show"], cleanup=True)

    def test_restore_ack_uncertain_blocks_future_uart(self):
        def restore_unknown(_m, _engine, value, cleanup=False):
            self.write(_m, _engine, value, cleanup)
            if value == 0:
                raise ValueError("I0恢复ACK失败")
        with self.assertRaisesRegex(ValueError, "I对照失败"):
            self.run_candidate(writer=restore_unknown)
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)
        self.assertEqual(self.engine.rows[0]["status"], "FAIL")

    def test_entry_torque_nonzero_blocks_all_later_uart_without_i_write(self):
        self.values["Torque_Enable"] = 1
        with self.assertRaisesRegex(ValueError, "入口停止未确认"):
            self.run_candidate()
        self.assertEqual(self.events, [("gate",), ("read", "Torque_Enable", False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command(["position-tool", "read"], cleanup=True)

    def test_entry_velocity_read_unknown_blocks_all_later_uart_without_i_write(self):
        def unknown(m, engine, reg, cleanup=False):
            if reg == "Present_Velocity":
                raise ValueError("入口PV读回未知")
            return self.read(m, engine, reg, cleanup)
        with self.assertRaisesRegex(ValueError, "入口PV读回未知"):
            c.run_trial(None, self.engine, *c.TARGET, {}, self.original, reader=unknown, writer=self.write)
        self.assertFalse(any(e[0] in ("write", "original_position") for e in self.events))
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command([c.RAW_TOOL["path"], "read"], cleanup=True)

    def test_restore_lock_zero_blocks_i0_write_and_future_uart(self):
        def lock_changed(engine, *args):
            self.original(engine, *args)
            self.values["Lock"] = 0
        with self.assertRaisesRegex(ValueError, "I对照失败"):
            self.run_candidate(original=lock_changed)
        self.assertEqual([e for e in self.events if e[0] == "write"], [("write", 2, False)])
        self.assertTrue(self.engine.manual_cut)
        with self.assertRaisesRegex(ValueError, "粘性拒绝"):
            self.io.command([c.RAW_TOOL["path"], "read"], cleanup=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
