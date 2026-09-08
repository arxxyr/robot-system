#!/usr/bin/env -S uv run --no-project python
"""核对本轮 ELF 产物的 preinit/init 数组，不把源码属性当作链接结果。

支持 Linux ELF64 小端 x86_64/AArch64；拒绝未知格式。执行顺序依据 ELF gABI：
https://gabi.xinuos.com/elf/08-dynamic.html#initialization-and-termination-functions
这里只证明测量起点覆盖初始化数组，不测加载器耗时，也不替代实跑的10ms包络。
"""

import argparse
import hashlib
import json
import struct
import subprocess
from pathlib import Path


def inspect(path: Path) -> dict:
    """解析数组、重定位和符号，要求最先的 preinit 就是本轮观测器。"""
    blob = path.read_bytes()
    if blob[:6] != b"\x7fELF\x02\x01":
        raise ValueError("仅支持 ELF64 小端产物")
    machine = struct.unpack_from("<H", blob, 18)[0]
    relative_type = {62: 8, 183: 1027}.get(machine)
    if relative_type is None:
        raise ValueError(f"未支持的 ELF machine={machine}")
    section_offset = struct.unpack_from("<Q", blob, 40)[0]
    entry_size, count, names_index = struct.unpack_from("<HHH", blob, 58)
    if entry_size != 64 or not count or names_index >= count:
        raise ValueError("section table 不完整或使用未支持的扩展索引")
    sections = [struct.unpack_from("<IIQQQQIIQQ", blob, section_offset + i * entry_size)
                for i in range(count)]
    strings = sections[names_index]
    names = blob[strings[4]:strings[4] + strings[5]]
    by_name = {}
    for section in sections:
        name = names[section[0]:].split(b"\0", 1)[0].decode()
        by_name[name] = section

    symbols = {}
    for section in sections:
        if section[1] not in (2, 11):
            continue
        if section[9] != 24 or section[6] >= count:
            raise ValueError("符号表项格式无效")
        linked = sections[section[6]]
        symbol_strings = blob[linked[4]:linked[4] + linked[5]]
        for offset in range(section[4], section[4] + section[5], 24):
            name_offset, info, _, defined_section, address, _ = struct.unpack_from("<IBBHQQ", blob, offset)
            name = symbol_strings[name_offset:].split(b"\0", 1)[0].decode()
            if name and defined_section != 0 and address != 0 and info & 0xF == 2:
                symbols.setdefault(address, []).append(name)

    relocations = {}
    for section in sections:
        if section[1] not in (4, 9):
            continue
        size = 24 if section[1] == 4 else 16
        if section[9] != size or section[5] % size:
            raise ValueError("REL/RELA 项大小无效")
        for offset in range(section[4], section[4] + section[5], size):
            target, info = struct.unpack_from("<QQ", blob, offset)
            addend = struct.unpack_from("<q", blob, offset + 16)[0] if size == 24 else None
            if target in relocations:
                raise ValueError(f"同一槽重复重定位：{target:#x}")
            relocations[target] = (info & 0xFFFFFFFF, info >> 32, addend)

    def pointers(name: str) -> list[dict]:
        section = by_name.get(name)
        if section is None:
            return []
        if section[5] % 8:
            raise ValueError(f"{name} 长度不是指针宽度的倍数")
        result = []
        for index in range(section[5] // 8):
            address = section[3] + index * 8
            raw = struct.unpack_from("<Q", blob, section[4] + index * 8)[0]
            relocation = relocations.get(address)
            if relocation is not None:
                kind, symbol_index, addend = relocation
                if kind != relative_type or symbol_index != 0 or addend is None:
                    raise ValueError(f"{name}[{index}] 未支持的重定位：{relocation}")
                target = addend
            else:
                target = raw
            if target <= 0 or target not in symbols:
                raise ValueError(f"{name}[{index}] 未解析到已定义的非空函数：{target:#x}")
            result.append({"index": index, "slot": hex(address), "target": hex(target),
                           "relocation_type": relocation[0] if relocation else None,
                           "symbols": symbols.get(target, [])})
        return result

    preinit = pointers(".preinit_array")
    init = pointers(".init_array")
    if not preinit or not any("observe_preinit" in name for name in preinit[0]["symbols"]):
        raise ValueError("preinit首项未解析到本轮observe_preinit，不能宣称覆盖ctor")
    if not init or any(not item["symbols"] for item in init):
        raise ValueError("init数组为空或存在无法归属的函数")
    dynamic_values = {}
    for section in sections:
        if section[1] == 6:
            for offset in range(section[4], section[4] + section[5], 16):
                tag, value = struct.unpack_from("<qQ", blob, offset)
                if tag == 0:
                    break
                dynamic_values[tag] = value
    for name, address_tag, size_tag in [(".preinit_array", 32, 33), (".init_array", 25, 27)]:
        section = by_name[name]
        if (dynamic_values.get(address_tag), dynamic_values.get(size_tag)) != (section[3], section[5]):
            raise ValueError(f"动态表{name}地址/长度与已审计section不符")
    dynamic = subprocess.run(["readelf", "--wide", "--dynamic", str(path)],
                             check=True, capture_output=True, text=True).stdout
    if "(PREINIT_ARRAY)" not in dynamic or "(PREINIT_ARRAYSZ)" not in dynamic:
        raise ValueError("动态表未发布preinit数组，section属性不能替代实际加载契约")
    return {"status": "PASS", "binary": str(path.resolve()),
            "sha256": hashlib.sha256(blob).hexdigest(), "machine": machine,
            "preinit": preinit, "init": init, "dynamic": dynamic,
            "scope": "初始化边界覆盖证明；10ms判据由真实观测页单独验证",
            "ordering_source": "https://gabi.xinuos.com/elf/08-dynamic.html"}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        payload = inspect(args.binary)
    except (OSError, ValueError, struct.error, subprocess.CalledProcessError) as error:
        payload = {"status": "FAIL", "binary": str(args.binary), "error": str(error)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": payload["status"], "report": str(args.output)}, ensure_ascii=False))
    raise SystemExit(0 if payload["status"] == "PASS" else 1)


if __name__ == "__main__":
    main()
