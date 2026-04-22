#!/usr/bin/env bash
# 采集实测环境信息，生成 docs/10_verification.md 附录 "实测环境登记" 块。
#
# 用法：
#   bash scripts/collect_env_id.sh            # 打印登记条到 stdout
#   bash scripts/collect_env_id.sh >> ...     # 附加到文件
#
# 覆盖平台：macOS (Darwin) + Linux。WSL 等走 Linux 分支。
#
# env_id 命名约定（见 docs/10_verification.md §0 "run_env 约定"）：
#   <os_label>-<arch>-<date>
# 例：macos-15.4-arm64-20260422 / ubuntu-22.04-x86_64-20260501

set -euo pipefail

# ---- OS + 内核 ----
uname_s="$(uname -s)"
arch="$(uname -m)"

case "${uname_s}" in
    Darwin)
        os_label="macos-$(sw_vers -productVersion 2>/dev/null || echo unknown)"
        kernel="Darwin $(uname -r) ($(sw_vers -buildVersion 2>/dev/null || echo ?))"
        ;;
    Linux)
        if [ -r /etc/os-release ]; then
            # shellcheck disable=SC1091
            . /etc/os-release
            os_label="${ID:-linux}-${VERSION_ID:-unknown}"
        else
            os_label="linux-unknown"
        fi
        kernel="$(uname -sr)"
        ;;
    *)
        os_label="$(echo "${uname_s}" | tr '[:upper:]' '[:lower:]')-unknown"
        kernel="$(uname -sr)"
        ;;
esac

# ---- CPU governor（Linux 专有）----
governor="—"
if [ "${uname_s}" = "Linux" ] && [ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]; then
    governor="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
fi

# ---- rustc 版本 ----
if command -v rustc >/dev/null 2>&1; then
    rustc_ver="$(rustc -V)"
else
    rustc_ver="rustc: not installed on PATH"
fi

# ---- iceoryx2 版本（从 Cargo.lock 取）----
# 优先从仓库根的 Cargo.lock 读；不存在则记 "n/a"。
ice_ver="n/a"
lock_file=""
if [ -r Cargo.lock ]; then
    lock_file="Cargo.lock"
elif [ -r "$(dirname "$0")/../Cargo.lock" ]; then
    lock_file="$(dirname "$0")/../Cargo.lock"
fi
if [ -n "${lock_file}" ]; then
    # 典型片段：
    #   [[package]]
    #   name = "iceoryx2"
    #   version = "=0.8.1"
    #   source = "registry+https://..."
    ice_ver="$(awk '
        /^name = "iceoryx2"$/ { found=1; next }
        found && /^version = / {
            gsub(/"/, "", $3); print $3; exit
        }
    ' "${lock_file}" 2>/dev/null)"
    [ -z "${ice_ver}" ] && ice_ver="n/a"
fi

# ---- 宿主机 hostname ----
host_name="$(hostname 2>/dev/null || echo unknown)"

# ---- 日期（UTC 年月日）----
date_stamp="$(date -u +%Y%m%d)"

env_id="${os_label}-${arch}-${date_stamp}"

# ---- 输出登记条（与 docs/10_verification.md 附录格式一致）----
cat <<EOF
env_id        ${env_id}
host          ${host_name} · ${arch}
os            ${kernel}
cpu governor  ${governor}
iceoryx2      ${ice_ver}
rust toolchain ${rustc_ver}
测得项        (实测时在此填)
备注           (实测时在此填)
EOF
