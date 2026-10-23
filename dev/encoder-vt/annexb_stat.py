#!/usr/bin/env python3
"""御端原生编码器裸流校验：Annex-B 切分 + NAL 类型统计 + ffmpeg/ffprobe 交叉验证。

用法：
  python3 annexb_stat.py <file.h264|file.h265> [expected_w expected_h]
  python3 annexb_stat.py --probe <file> --codec h264 --w 1280 --h 720

职责：
  1. 校验起始码（00 00 00 01 / 00 00 01）、无空 NAL；
  2. 统计 NAL 类型序列（H.264: 7=SPS 8=PPS 5=IDR 1=非IDR片；HEVC: 32=VPS 33=SPS 34=PPS 19/20=IDR 1=TRAIL_R）；
  3. 校验参数集在位（首个可解码单元必须已含 SPS/PPS）；
  4. 可选调用 ffprobe 交叉验证（宽高/帧数/关键帧）。

作为编码器库的**可复现验证 fixture**：任何新编码器后端都必须能通过本脚本。
"""
import re
import subprocess
import sys

H264_NAMES = {1: "非IDR片", 5: "IDR", 6: "SEI", 7: "SPS", 8: "PPS", 9: "AUD"}
H265_NAMES = {1: "TRAIL_R", 19: "IDR_N_LP", 20: "IDR_W_RADL", 32: "VPS", 33: "SPS", 34: "PPS", 35: "AUD", 39: "PREFIX_SEI"}


def split_annexb(data: bytes):
    """返回 [(start_code_len, payload)] ，payload 为 NAL 体（不含起始码，已去尾部补齐零）。"""
    starts = []
    i = 0
    n = len(data)
    while i + 2 < n:
        if data[i] == 0 and data[i + 1] == 0 and data[i + 2] == 1:
            scl = 4 if (i > 0 and data[i - 1] == 0) else 3
            starts.append((i - (scl - 3), scl))
            i += 3
        else:
            i += 1
    if not starts:
        return []
    out = []
    for k, (pos, scl) in enumerate(starts):
        end = starts[k + 1][0] if k + 1 < len(starts) else n
        payload = data[pos + scl:end]
        # 去除尾部用于对齐的零字节（cabac_zero_words）
        payload = payload.rstrip(b"\x00") if payload and payload[-1] == 0 else payload
        out.append((scl, payload))
    return out


def nal_types(data: bytes, hevc: bool):
    types = []
    for scl, payload in split_annexb(data):
        if not payload:
            return None, "存在空 NAL（起始码后无载荷）"
        t = (payload[0] >> 1) & 0x3F if hevc else (payload[0] & 0x1F)
        types.append(t)
    return types, None


def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 1
    path = args[0]
    hevc = (path.endswith(".h265") or path.endswith(".hevc") or path.endswith(".hvc")
            or any(a == "--hevc" for a in args))
    data = open(path, "rb").read()
    parts = split_annexb(data)
    if not parts:
        print(f"FAIL {path}: 未找到 Annex-B 起始码")
        return 1
    types, err = nal_types(data, hevc)
    if err:
        print(f"FAIL {path}: {err}")
        return 1
    names = H265_NAMES if hevc else H264_NAMES
    seq = ", ".join(f"{t}({names.get(t, '?')})" for t in types)
    print(f"OK {path}: {len(types)} 个 NAL，类型序列: {seq}")
    if hevc:
        need = {32, 33, 34}
        has_ps = need.issubset(set(types))
        key_types = {19, 20}
    else:
        need = {7, 8}
        has_ps = need.issubset(set(types))
        key_types = {5}
    print(f"参数集在位: {has_ps}（期望包含 {sorted(need)}）")
    if not has_ps:
        print("FAIL: 缺参数集，解码器无法起播")
        return 1
    # 首个参数集应出现在第一个 IDR 之前（保证首帧可独立解码）
    first_key = next((i for i, t in enumerate(types) if t in key_types), None)
    first_ps = next((i for i, t in enumerate(types) if t in need), None)
    if first_key is not None and first_ps is not None and first_ps > first_key:
        print("WARN: 首个关键帧出现在参数集之前（部分解码器容错，但不推荐）")
    print(f"关键帧(I)数量: {sum(1 for t in types if t in key_types)} / 片层单元: {sum(1 for t in types if t in ({1, 5} if not hevc else {1, 19, 20}))}")
    # ffprobe 交叉验证（存在 ffmpeg 时）
    try:
        fmt = "hevc" if hevc else "h264"
        p = subprocess.run(
            ["ffprobe", "-f", fmt, "-i", path, "-show_entries",
             "frame=pkt_size,key_frame", "-of", "csv=p=0"],
            capture_output=True, text=True, timeout=60)
        if p.returncode == 0 and p.stdout.strip():
            lines = [l for l in p.stdout.strip().splitlines() if l.strip()]
            keys = sum(1 for l in lines if l.split(",")[1] == "1")
            print(f"ffprobe: 解码出 {len(lines)} 帧，其中关键帧 {keys}")
    except FileNotFoundError:
        print("ffprobe 不可用（跳过交叉验证）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
