#!/usr/bin/env python3
"""P0-6 主机侧验收解码器（最小范围）。

只做两件事：
  1) 核对布局：从组件源码的布局标记提取结构体，用宿主 cc 编译取 offsetof/sizeof；
  2) 用固定字节样本判定：完整记录 / magic 错 / nonce 错 / CLAIMED 未完成 / 核号错 / 字段不符。

**边界**：这些测试不证明目标上的 CAS、重入或异常路径安全；也不能替代真机 V2。
"""
import argparse, os, re, struct, subprocess, sys, tempfile, pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
SRC = ROOT / "rust-firmware/components/p06_recorder/recorder.c"

MAGIC = 0x50303631
ST_EMPTY, ST_CLAIMED, ST_DONE = 0x00000000, 0x434C4149, 0x444F4E45
SLOTS = 8

# V2 期望值（PC 必须来自最终验证 ELF，由 --elf-pc 重新提取后再填入）
EXPECT = {"pc": None, "excvaddr": 0, "cause": 29}   # cause 仅为待验预期


def extract_layout():
    t = SRC.read_text()
    m = re.search(r"/\* P06_LAYOUT_BEGIN.*?\*/(.*?)/\* P06_LAYOUT_END \*/", t, re.S)
    if not m:
        sys.exit("未找到 P06_LAYOUT_BEGIN/END 标记")
    return m.group(1)


def build_offsets():
    body = extract_layout()
    fields = ["state", "core", "cause", "seq", "f_pc", "f_ps", "f_a0", "f_a1",
              "f_exccause", "f_excvaddr", "frame_ptr", "prev_handler", "live_ps",
              "panic_abort_details"]
    lines = ["#include <stdio.h>", "#include <stddef.h>", body, "int main(void){"]
    lines.append('  printf("frame.size %zu\\n", sizeof(p06_frame_t));')
    lines.append('  printf("rec.size %zu\\n", sizeof(p06_rec_t));')
    lines.append('  printf("region.size %zu\\n", sizeof(p06_region_t));')
    lines.append('  printf("region.prev %zu\\n", offsetof(p06_region_t, prev));')
    lines.append('  printf("region.slots %zu\\n", offsetof(p06_region_t, slots));')
    for f in fields:
        lines.append('  printf("rec.%s %%zu\\n", offsetof(p06_rec_t, %s));' % (f, f))
    lines.append("  return 0;\n}")
    with tempfile.TemporaryDirectory() as d:
        c, exe = os.path.join(d, "l.c"), os.path.join(d, "l")
        pathlib.Path(c).write_text("\n".join(lines))
        r = subprocess.run(["cc", "-w", c, "-o", exe], capture_output=True, text=True)
        if r.returncode:
            sys.exit("布局编译失败：\n" + r.stderr[:400])
        out = subprocess.run([exe], capture_output=True, text=True).stdout
    off = {}
    for ln in out.splitlines():
        k, v = ln.split()
        off[k] = int(v)
    return off


def decode(region, off, nonce):
    """返回 (verdict, detail)"""
    def u(a):
        return struct.unpack_from("<I", region, a)[0]
    if u(0) != MAGIC:
        return "BAD_MAGIC", {"magic": hex(u(0))}
    if u(4) != nonce:
        return "STALE_NONCE", {"nonce": hex(u(4)), "expect": hex(nonce)}
    rs = off["rec.size"]
    base = off["region.slots"]
    seen = []
    for ci in (0, 1):
        r = base + ci * rs
        st = u(r + off["rec.state"])
        if st == ST_EMPTY:
            continue
        rec = {
            "slot": ci, "state": hex(st), "core": u(r + off["rec.core"]),
            "cause": u(r + off["rec.cause"]),
            "f_pc": u(r + off["rec.f_pc"]), "f_excvaddr": u(r + off["rec.f_excvaddr"]),
            "f_exccause": u(r + off["rec.f_exccause"]),
            "prev_handler": u(r + off["rec.prev_handler"]),
            "panic_abort_details": u(r + off["rec.panic_abort_details"]),
        }
        if st == ST_CLAIMED:
            return "INCOMPLETE", rec
        if rec["core"] != ci:
            return "WRONG_CORE", rec
        if EXPECT["pc"] is not None and rec["f_pc"] != EXPECT["pc"]:
            return "FIELD_MISMATCH", {"field": "f_pc", **rec}
        if rec["f_excvaddr"] != EXPECT["excvaddr"]:
            return "FIELD_MISMATCH", {"field": "f_excvaddr", **rec}
        if rec["f_exccause"] != EXPECT["cause"]:
            return "FIELD_MISMATCH", {"field": "f_exccause", **rec}
        seen.append(rec)
    if not seen:
        return "NO_RECORD", {}
    return "OK", {"records": seen}


def mk_region(off, nonce, ci, state, core, pc):
    buf = bytearray(off["region.size"])
    struct.pack_into("<II", buf, 0, MAGIC, nonce)
    r = off["region.slots"] + ci * off["rec.size"]
    struct.pack_into("<I", buf, r + off["rec.state"], state)
    struct.pack_into("<I", buf, r + off["rec.core"], core)
    struct.pack_into("<I", buf, r + off["rec.cause"], EXPECT["cause"])
    struct.pack_into("<I", buf, r + off["rec.f_pc"], pc)
    struct.pack_into("<I", buf, r + off["rec.f_excvaddr"], EXPECT["excvaddr"])
    struct.pack_into("<I", buf, r + off["rec.f_exccause"], EXPECT["cause"])
    struct.pack_into("<I", buf, r + off["rec.prev_handler"], 0x4037650C)
    return bytes(buf)


def selftest(off, pc):
    EXPECT["pc"] = pc
    nonce = 0x12345679
    cases = [
        ("完整记录", mk_region(off, nonce, 0, ST_DONE, 0, pc), nonce, "OK"),
        ("magic 错", bytearray(mk_region(off, nonce, 0, ST_DONE, 0, pc)), nonce, "BAD_MAGIC"),
        ("nonce 错(旧内容)", mk_region(off, 0xAAAAAAAA, 0, ST_DONE, 0, pc), nonce, "STALE_NONCE"),
        ("CLAIMED 未完成", mk_region(off, nonce, 0, ST_CLAIMED, 0, pc), nonce, "INCOMPLETE"),
        ("核号错", mk_region(off, nonce, 1, ST_DONE, 0, pc), nonce, "WRONG_CORE"),
        ("字段不符(PC)", mk_region(off, nonce, 0, ST_DONE, 0, pc ^ 4), nonce, "FIELD_MISMATCH"),
        ("空记录区", bytes(off["region.size"]), nonce, "BAD_MAGIC"),
    ]
    bad = 0
    print("布局（宿主 cc 实测）：frame=%d  rec=%d  region=%d  slots@%d"
          % (off["frame.size"], off["rec.size"], off["region.size"], off["region.slots"]))
    for name, buf, n, want in cases:
        if name == "magic 错":
            buf = bytearray(buf); struct.pack_into("<I", buf, 0, 0xDEADBEEF)
        got, detail = decode(bytes(buf), off, n)
        ok = got == want
        bad += 0 if ok else 1
        print("  [%s] %-16s -> %-14s %s" % ("PASS" if ok else "FAIL", name, got,
                                            "" if ok else ("期望 " + want + " " + str(detail))))
    print("自检：%d/%d 通过" % (len(cases) - bad, len(cases)))
    return 1 if bad else 0


def elf_trigger_pc(elf):
    od = os.path.expanduser("~/.espressif/tools/xtensa-esp-elf/esp-14.2.0_20260121/"
                            "xtensa-esp-elf/bin/xtensa-esp32s3-elf-objdump")
    out = subprocess.run([od, "-d", "--disassemble=p06_v2_trigger", elf],
                         capture_output=True, text=True).stdout
    for ln in out.splitlines():
        if ("l32i" in ln or "s32i" in ln) and "a8" in ln:
            return int(ln.split(":")[0].strip(), 16)
    sys.exit("未能从 ELF 提取 p06_v2_trigger 的故障指令地址")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--layout", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--elf", default=str(ROOT / "rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4"))
    ap.add_argument("--dump")
    ap.add_argument("--vaddr", default="0")
    ap.add_argument("--cause", default="29")
    ap.add_argument("--nonce", default=None)
    a = ap.parse_args()
    off = build_offsets()
    if a.layout:
        for k in sorted(off):
            print("  %-16s %d" % (k, off[k]))
        return 0
    EXPECT["excvaddr"] = int(a.vaddr, 0)
    EXPECT["cause"] = int(a.cause, 0)
    if a.selftest:
        return selftest(off, 0x42123456)
    if a.dump:
        pc = elf_trigger_pc(a.elf)
        print("从最终 ELF 提取的期望 PC = 0x%08X（%s）" % (pc, a.elf))
        EXPECT["pc"] = pc
        raw = pathlib.Path(a.dump).read_bytes()
        n = int(a.nonce, 0) if a.nonce else struct.unpack_from("<I", raw, 4)[0]
        v, d = decode(raw, off, n)
        print(v, d)
        return 0 if v == "OK" else 1
    ap.print_help()
    return 0


if __name__ == "__main__":
    sys.exit(main())
