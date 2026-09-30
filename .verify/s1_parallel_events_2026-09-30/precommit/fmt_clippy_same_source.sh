set -eu
export PATH=/home/loosqk.guest/.cargo/bin:/home/loosqk.guest/.local/bin:$PATH
export RUSTUP_TOOLCHAIN=nightly
export RUSTC_WRAPPER=
export CARGO_TARGET_DIR=/home/loosqk.guest/robot-checks/build
export CARGO_BUILD_JOBS=2
cd /home/loosqk.guest/robot-checks/s1-parallel-events-20260930-r2/source
uv run --no-project python - <<'CHECK_PY'
from pathlib import Path
import subprocess,sys,json
sys.dont_write_bytecode=True
sys.path.insert(0,'scripts')
from post_e_h2_artifacts import snapshot_source
names=subprocess.check_output(['git','ls-files','-z']).decode().rstrip('\0').split('\0')
s=snapshot_source(Path.cwd(),names)
assert len(s['files'])==589 and s['source_manifest_sha256']=='72ac7a50c9aeddaa0ac47f21543f3c21e3f05a2c72e7b7e3108246b2d6e4cc02'
print(json.dumps({'before_input_count':589,'before_source_sha256':s['source_manifest_sha256']}))
CHECK_PY
rustc -Vv
cargo fmt --all
cargo clippy --locked --offline --all --all-targets -- -D warnings
uv run --no-project python - <<'CHECK_PY'
from pathlib import Path
import subprocess,sys,json
sys.dont_write_bytecode=True
sys.path.insert(0,'scripts')
from post_e_h2_artifacts import snapshot_source
names=subprocess.check_output(['git','ls-files','-z']).decode().rstrip('\0').split('\0')
s=snapshot_source(Path.cwd(),names)
assert len(s['files'])==589 and s['source_manifest_sha256']=='72ac7a50c9aeddaa0ac47f21543f3c21e3f05a2c72e7b7e3108246b2d6e4cc02'
print(json.dumps({'after_input_count':589,'after_source_sha256':s['source_manifest_sha256']}))
CHECK_PY
rustc -Vv
