# 御端主端原生解码器（macOS / VideoToolbox）

实现 protocol.md **§8.3.18** 的解码器 C-ABI，与副端编码器（§8.3.3，
`dev/encoder-vt/`）对称。主端经 `YUDUAN_DECODER_LIB` 指向本库，运行时 dlopen 装载。

## 契约

```c
void*    yuduan_dec_create(const char* codec, uint32_t width, uint32_t height);
uint32_t yuduan_dec_decode(void* h, const uint8_t* data, uint32_t len,
                           uint8_t* out, uint32_t out_cap);
uint32_t yuduan_dec_out_size(void* h);
void     yuduan_dec_destroy(void* h);
```

| 项 | 约定 |
|---|---|
| codec | 仅 `h264` / `h265`；其余返回 `NULL`（**不虚标**能力） |
| 输入 | Annex-B（`00 00 00 01` / `00 00 01` 起始码） |
| 输出 | RGBA8，紧凑 `w*h*4` |
| 返回 | 写入字节数；`0` = 本帧未出图（等下一个关键帧），**不得**以黑帧冒充 |
| 起播 | 参数集从码流提取建 `CMVideoFormatDescription`，无需带外协商 |

## 构建

```bash
bash build.sh                      # → /tmp/libyuduan_dec_vt.dylib
bash build.sh /path/to/out.dylib
```

依赖框架：VideoToolbox / CoreMedia / CoreFoundation / CoreVideo（系统自带，零第三方）。

## 自检（编码 → 解码真实往返）

```bash
clang -O2 harness.c -o /tmp/dec_harness
/tmp/dec_harness ../../dev/encoder-vt/libyuduan_enc_vt.dylib /tmp/libyuduan_dec_vt.dylib
```

覆盖：C-ABI 符号齐全性、`create` 对不支持 codec 拒接、`out_size == w*h*4`、
垃圾输入返回 0、**真实往返**（已知 RGBA → VT 编码 → VT 解码 → 逐像素比对）。

实测（640×360，12 帧）：解码均值 R=126.3（原图 127.5），MAD=7.1，非纯色
——排除「返回全 0 缓冲冒充成功」。

## 环境变量

| 变量 | 作用 |
|---|---|
| `YUDUAN_DEC_DEBUG=1` | 打印会话创建/参数集/出帧各阶段 OSStatus（排障用，生产关闭） |
| `YUDUAN_DEC_PF=bgra\|nv12` | 强制输出像素格式（默认 BGRA 优先、失败降级 NV12） |

## 三平台状态（铁律 2）

| 平台 | 后端 | 状态 |
|---|---|---|
| macOS | VideoToolbox | ✅ 本机端到端验证（P2P 直连 + h264 1080p，70/70 帧） |
| Windows | Media Foundation（Sync MFT reader） | ⏳ 待实机（库路径与 ABI 一致，替换后端即可） |
| Linux | VAAPI | ⏳ 待实机（同上） |

未挂载/装载失败的库：主端诚实回落 JPEG 并发 `notice`，**不选**原生码流。

## 端到端验收

```bash
python3 ../../dev/e2e_tauri_p2p.py
```

真实 Go 中枢 + Rust 副端（真实屏幕采集 + VT 编码）+ 主端 P2P 直连 + 本库解码，9/9。
