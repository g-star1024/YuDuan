# encoder-mf — Windows Media Foundation 原生编码器（§8.3.14）

与 `encoder-vt`（macOS）/ `encoder-vaapi`（Linux）同构的 §8.3.3 C-ABI 动态库，
供副端 `YUDUAN_ENCODER_LIB` 运行时加载。仅支持 h264（微软系统自带 H.264 编码
MFT，Windows 8+ 无需额外安装；其余 codec `create` 返回 NULL，副端诚实回退）。

## 验证状态（诚实标注）

| 环节 | 状态 |
|---|---|
| 语法/类型（mfstub.h 声明子集 `-fsyntax-only`） | ✅ macOS 开发机可跑（`bash check.sh`） |
| 运行时编码验证 | ⏳ 待 Windows 实机 |

实机验证步骤（"x64 Native Tools Command Prompt"）：

```cmd
build-windows.cmd
set YUDUAN_ENCODER_LIB=%TEMP%\yuduan_enc_mf.dll
:: 启动副端，能力宣告应含 h264；或跑仓库内 e2e_vt_encoder.py（同脚本复用）
```

## 实现要点

- 枚举：`MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, SYNCMFT, out=H264)`，无编码器
  → `create` 返回 NULL（探测诚实失败）。异步 MFT（SYNCMFT 枚举不到的）不覆盖，
  可在实机按需加 `MFT_ENUM_FLAG_ASYNCMFT` + 事件驱动改造。
- 类型协商：先 SetOutputType（GetOutputAvailableType 找 H264 子型 + 设码率/Profile），
  再 SetInputType（NV12 渐进，30fps）——编码器 MFT 要求先输出后输入。
- 码控：CBR（`YUDUAN_ENC_KBPS` 默认 4000kbps）；GOP `YUDUAN_ENC_GOP` 默认 120，
  关键帧经 `ICodecAPI CODECAPI_AVEncVideoForceKeyFrame` 强推（首帧必推）。
- SPS/PPS：MS H264 MFT 在关键帧 in-band 自带（Annex-B），无需手工前置。
- 出帧：ProcessInput 后 ProcessOutput 拉取；首帧前 `NEED_MORE_INPUT` 属正常
  （MFT 管线缓存），该轮 encode 返回 0 且无 pending，副端按丢弃重采处理。
- COM：create 中 `CoInitializeEx(MULTITHREADED)` + `MFStartup`；destroy 对称释放。
  注意契约要求 create/encode/destroy 同线程调用（COM 单元约束）。

## 实机排障入口

- `YUDUAN_ENC_DEBUG=1` 打印各阶段 HRESULT。
- 常见失败：N 版 Windows 缺媒体特性包（无 H.264 编码 MFT）；
  远程会话/无 GPU 时 MS 软编码器仍可用（MFT 是系统组件）。
- mfstub.h 仅离线自检用（GUID 值据 MSDN）；目标机以 SDK 真头为准。
