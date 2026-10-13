"""爸爸，AST/bash语法与未绑定首门；禁止任何远端命令或真实现场访问。"""

import ast
import base64
import hashlib
import json
from pathlib import Path
import shlex
import subprocess
import time
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
generation = json.loads((HERE / "runtime-draft-generation-receipt.json").read_text())
rows = []
children = []


def python_body(source):
    if "<<'PYREMOTE'\n" in source:
        return source.split("<<'PYREMOTE'\n", 1)[1].rsplit("\nPYREMOTE\n", 1)[0]
    marker = "exec /home/loosqk/.local/share/uv/python/cpython-3.12.13-linux-aarch64-gnu/bin/python3 -c "
    if marker in source:
        args = shlex.split(source.split(marker, 1)[1])
        assert len(args) == 1
        return args[0]
    return None


for entry in generation["drafts"]:
    path = Path(entry["path"])
    raw = path.read_bytes()
    assert hashlib.sha256(raw).hexdigest() == entry["sha256"] and len(raw) == entry["bytes"]
    source = raw.decode()
    body = source if path.suffix == ".py" else python_body(source)
    if body is not None:
        tree = ast.parse(body)
        error = None
        count = {"child": 0, "file": 0}

        def child_block(*args, **kwargs):
            count["child"] += 1
            raise RuntimeError("爸爸，未绑定首门不应启动child。")

        def file_block(*args, **kwargs):
            count["file"] += 1
            raise RuntimeError("爸爸，未绑定首门不应读写目标文件。")

        with patch("subprocess.Popen", side_effect=child_block), patch("subprocess.run", side_effect=child_block), \
             patch("pathlib.Path.read_bytes", side_effect=file_block), patch("pathlib.Path.read_text", side_effect=file_block), \
             patch("pathlib.Path.mkdir", side_effect=file_block), patch("os.open", side_effect=file_block), \
             patch("os.execv", side_effect=child_block):
            try:
                exec(compile(tree, str(path), "exec"), {"__name__": "unbound_runtime_only", "__file__": str(path)})
            except AssertionError as caught:
                error = str(caught)
        assert error and count == {"child": 0, "file": 0}, (path, count, error)
        none_fields = [n.targets[0].id for n in tree.body if isinstance(n, ast.Assign) and len(n.targets) == 1
                       and isinstance(n.targets[0], ast.Name) and isinstance(n.value, ast.Constant) and n.value.value is None]
        rows.append({"path": str(path), "AST": "PASS", "unbound_first_gate": "REJECTED_BEFORE_FILE_OR_CHILD",
                     "reason": error, "guard_child_calls": 0, "guard_target_file_calls": 0, "None_fields": none_fields})
    if path.suffix == ".sh":
        started = time.time_ns()
        child = subprocess.Popen(["bash", "-n", str(path)], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stdout, stderr = child.communicate()
        code = child.wait()
        receipt = {"name": path.name + "-syntax", "pid": child.pid, "argv": ["bash", "-n", str(path)],
                   "started_ns": started, "finished_ns": time.time_ns(), "exit_code": code, "wait_returncode": code,
                   "reaped": True, "signals_sent": [], "stdout": stdout.decode(), "stderr": stderr.decode()}
        children.append(receipt)
        assert code == 0 and stdout == stderr == b""

observer = HERE / "observe_base_feedback_diagnostic_once_r1.draft.sh"
started = time.time_ns()
child = subprocess.Popen(["bash", str(observer)], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
stdout, stderr = child.communicate()
code = child.wait()
children.append({"name": "observer-none-first-gate", "pid": child.pid, "argv": ["bash", str(observer)],
                 "started_ns": started, "finished_ns": time.time_ns(), "exit_code": code, "wait_returncode": code,
                 "reaped": True, "signals_sent": [], "stdout": stdout.decode(), "stderr": stderr.decode()})
assert code == 2 and stdout == stderr == b""

start_body = python_body((HERE / "start_base_feedback_diagnostic_once_r1.draft.sh").read_text())
tree = ast.parse(start_body)
launch_value = next(n.value for n in tree.body if isinstance(n, ast.Assign) and isinstance(n.targets[0], ast.Name) and n.targets[0].id == "launcher")
embedded = base64.b64decode(ast.literal_eval(launch_value.args[0]), validate=True)
assert embedded == (HERE / "launch_base_feedback_diagnostic_r1.draft.sh").read_bytes()
launcher_tree = ast.parse(python_body(embedded.decode()))
execv = next(n for n in launcher_tree.body if isinstance(n, ast.Expr) and isinstance(n.value, ast.Call)
             and isinstance(n.value.func, ast.Attribute) and n.value.func.attr == "execv").value
argv = ast.literal_eval(execv.args[1])
assert argv[1] == "/home/loosqk/robot-checks/base-feedback-r1-software/candidate/base_feedback_diagnostic_r1.py"
assert argv[argv.index("--artifact-root") + 1] == "/home/loosqk/robot-checks/base-feedback-diagnostic-r1"
assert argv[argv.index("--output") + 1] == "/home/loosqk/robot-checks/base-feedback-diagnostic-r1/base-feedback-diagnostic-r1"
assert "--execute" in argv and argv[argv.index("--approval") + 1].endswith("/base-feedback-diagnostic-r1.go.json")
result = {"status": "PASS_LOCAL_RUNTIME_DRAFT_SYNTAX_AND_NONE_GATES", "python_body_checks": rows,
          "bash_syntax_children": len(children) - 1, "observer_None_exit": code,
          "local_direct_children": children, "embedded_launcher_byte_equal": True, "launcher_argv": argv,
          "SSH_executions": 0, "UART_operations": 0, "field_created": False, "GO_created": False,
          "quality_batches": 0, "candidate_changed": False, "native_evidence_changed": False,
          "limitations": ["本机语法与None首门不证明Orin当前USB、供电、看护或新GO",
                          "新非UART入口只在root绑定恢复供电后的新现场回执才允许",
                          "只读诊断PASS不等于主动零速、物理停止或底盘三轮通过"]}
with (HERE / "runtime-draft-verification.json").open("x", encoding="utf-8") as stream:
    json.dump(result, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
print(json.dumps({"status": result["status"], "python_bodies": len(rows), "bash_syntax_children": len(children) - 1,
                  "observer_None_exit": code, "embedded_launcher_byte_equal": True,
                  "SSH_executions": 0, "UART_operations": 0}, ensure_ascii=False))
