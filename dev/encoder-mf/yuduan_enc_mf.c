/*
 * yuduan_enc_mf.c — 御端原生编码器（Windows Media Foundation 后端）
 *
 * 实现 protocol.md §8.3.3 C-ABI 契约（与 encoder-vt / encoder-vaapi 同构）：
 *   void*    yuduan_enc_create (const char* codec, uint32_t width, uint32_t height);
 *   uint32_t yuduan_enc_encode(void* h, const uint8_t* rgba, uint32_t rgba_len,
 *                              uint8_t* out, uint32_t out_cap);
 *   void     yuduan_enc_destroy(void* h);
 *
 * 行为：
 *   - 仅接受 "h264"（其余 codec 返回 NULL，能力探测不虚标）；
 *   - 输入 RGBA8，内部 BT.601 转 NV12 喂 MFT（微软 H.264 编码器输入要求 NV12）；
 *   - 输出 Annex-B（MS H264 MFT 在关键帧自带 SPS/PPS in-band，无需手工前置）；
 *   - CBR 码控（YUDUAN_ENC_KBPS 默认 4000），GOP 内 P 帧 + 周期强制关键帧
 *     （ICodecAPI CODECAPI_AVEncVideoForceKeyFrame）；
 *   - 返回写入字节数；out 不足返回 0（输出缓存在句柄内待重投，契约语义同 VT/VA）。
 *
 * ⚠️ 验证状态（诚实标注）：macOS 上用自写声明子集（mfstub.h）做离线语法/类型
 *    自检；**运行时验证待 Windows 实机**（MFT 枚举失败 create() 返回 NULL，
 *    副端诚实回退 RAW_RGBA）。实机排障见 README.md。
 *
 * 构建（Windows 目标机，见 build-windows.cmd）：SDK 头 <mfapi.h> <mfidl.h>
 * <codecapi.h> + 链接 mfplat.lib mfuuid.lib ole32.lib。
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#ifdef _WIN32
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfobjects.h>
#include <codecapi.h>
#include <combaseapi.h>
#else
#include "mfstub.h"
#endif

static int mf_debug(void) {
    static int init = 0, on = 0;
    if (!init) {
        const char *e = getenv("YUDUAN_ENC_DEBUG");
        on = (e && (strcmp(e, "1") == 0)) ? 1 : 0;
        init = 1;
    }
    return on;
}
#define DBG(...) do { if (mf_debug()) fprintf(stderr, "[enc_mf] " __VA_ARGS__); } while (0)

typedef struct {
    uint8_t *b;
    size_t n;
} outbuf;

typedef struct {
    IMFTransform *mft;
    ICodecAPI *codec;
    int com_init;       /* create 中 CoInitializeEx 成功标记 */
    int mf_started;
    int w, h;
    uint8_t *nv12;      /* RGBA->NV12 暂存（w*h*3/2） */
    int64_t frame;      /* 帧序（=100ns 时基换算） */
    int gop;
    outbuf out;
    int out_pending;
    int failed;
} mf_enc;

void yuduan_enc_destroy(void *handle);

/* RGBA -> NV12（BT.601 限幅，与 encoder-vaapi 同口径） */
static void rgba_to_nv12(const uint8_t *rgba, int w, int h,
                         uint8_t *yp, int ypitch, uint8_t *uvp, int uvpitch) {
    for (int y = 0; y < h; y++) {
        const uint8_t *row = rgba + (size_t)y * w * 4;
        uint8_t *yrow = yp + (size_t)y * ypitch;
        for (int x = 0; x < w; x++) {
            int r = row[x * 4], g = row[x * 4 + 1], b = row[x * 4 + 2];
            yrow[x] = (uint8_t)(((47 * r + 157 * g + 16 * b) >> 8) + 16);
        }
    }
    for (int y = 0; y < h / 2; y++) {
        const uint8_t *r0 = rgba + (size_t)(y * 2) * w * 4;
        const uint8_t *r1 = r0 + (size_t)w * 4;
        uint8_t *uvrow = uvp + (size_t)y * uvpitch;
        for (int x = 0; x < w / 2; x++) {
            int i = x * 8;
            int r = (r0[i] + r0[i + 4] + r1[i] + r1[i + 4]) >> 2;
            int g = (r0[i + 1] + r0[i + 5] + r1[i + 1] + r1[i + 5]) >> 2;
            int b = (r0[i + 2] + r0[i + 6] + r1[i + 2] + r1[i + 6]) >> 2;
            uvrow[x * 2]     = (uint8_t)(((-26 * r - 87 * g + 112 * b) >> 8) + 128);
            uvrow[x * 2 + 1] = (uint8_t)(((112 * r - 102 * g - 10 * b) >> 8) + 128);
        }
    }
}

static void mf_free_out(mf_enc *e) {
    free(e->out.b);
    e->out.b = NULL; e->out.n = 0;
}

/* 把 MF sample 的内容拷进 e->out（成功返回 0） */
static int sample_to_out(mf_enc *e, IMFSample *sample) {
    IMFMediaBuffer *buf = NULL;
    BYTE *data = NULL;
    DWORD len = 0, max = 0;
    if (sample->lpVtbl->ConvertToContiguousBuffer(sample, &buf) != S_OK) return -1;
    if (buf->lpVtbl->Lock(buf, &data, &max, &len) != S_OK) {
        buf->lpVtbl->Release(buf);
        return -1;
    }
    mf_free_out(e);
    e->out.b = malloc(len ? len : 1);
    if (e->out.b && len) memcpy(e->out.b, data, len);
    e->out.n = len;
    buf->lpVtbl->Unlock(buf);
    buf->lpVtbl->Release(buf);
    return e->out.b ? 0 : -1;
}

/* 选可用输出类型：遍历 GetOutputAvailableType 找 H264 子型，并补码率/Profile */
static HRESULT configure_output_type(mf_enc *e) {
    IMFMediaType *t = NULL;
    HRESULT hr;
    for (DWORD idx = 0; idx < 64; idx++) {
        hr = e->mft->lpVtbl->GetOutputAvailableType(e->mft, 0, idx, &t);
        if (hr != S_OK) return hr; /* 枚举尽未找到 */
        GUID sub;
        if (t->lpVtbl->GetGUID((IMFAttributes *)t, &MF_MT_SUBTYPE, &sub) == S_OK &&
            memcmp(&sub, &MFVideoFormat_H264, sizeof(GUID)) == 0) {
            t->lpVtbl->SetUINT32((IMFAttributes *)t, &MF_MT_AVG_BITRATE,
                                 (UINT32)(getenv("YUDUAN_ENC_KBPS")
                                          ? atoi(getenv("YUDUAN_ENC_KBPS")) * 1000 : 4000000));
            t->lpVtbl->SetUINT32((IMFAttributes *)t, &MF_MT_MPEG2_PROFILE,
                                 eAVEncH264VProfile_Base);
            hr = e->mft->lpVtbl->SetOutputType(e->mft, 0, t, 0);
            t->lpVtbl->Release(t);
            return hr;
        }
        t->lpVtbl->Release(t);
        t = NULL;
    }
    return E_FAIL;
}

void *yuduan_enc_create(const char *codec, uint32_t width, uint32_t height) {
    if (!codec || strcmp(codec, "h264") != 0 || width == 0 || height == 0 ||
        (width % 2) || (height % 2))
        return NULL;

    mf_enc *e = calloc(1, sizeof(mf_enc));
    if (!e) return NULL;
    e->w = (int)width; e->h = (int)height;
    e->gop = getenv("YUDUAN_ENC_GOP") ? atoi(getenv("YUDUAN_ENC_GOP")) : 120;
    e->nv12 = malloc((size_t)e->w * e->h * 3 / 2);
    if (!e->nv12) goto fail;

#ifndef _WIN32
    /* 非 Windows 仅做类型自检，不走真实 MF 初始化 */
    goto fail;
#else
    if (CoInitializeEx(NULL, COINIT_MULTITHREADED) == S_OK) e->com_init = 1;
    if (MFStartup(MF_VERSION, MFSTARTUP_LITE) == S_OK) e->mf_started = 1;
    else { DBG("MFStartup 失败"); goto fail; }

    /* 枚举同步 MFT 视频编码器（输出 H264） */
    MFT_REGISTER_TYPE_INFO out_info = { MFMediaType_Video, MFVideoFormat_H264 };
    IMFActivate **acts = NULL;
    UINT32 count = 0;
    if (MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_SYNCMFT,
                  NULL, &out_info, (void ***)&acts, &count) != S_OK || count == 0) {
        DBG("无可用 H264 编码器 MFT（能力探测失败，诚实回退）");
        if (acts) CoTaskMemFree(acts);
        goto fail;
    }
    HRESULT hr = acts[0]->lpVtbl->ActivateObject(acts[0], &IID_IMFTransform, (void **)&e->mft);
    for (UINT32 i = 0; i < count; i++) acts[i]->lpVtbl->Release(acts[i]);
    CoTaskMemFree(acts);
    if (hr != S_OK || !e->mft) { DBG("ActivateObject 失败"); goto fail; }

    if (configure_output_type(e) != S_OK) { DBG("SetOutputType(H264) 失败"); goto fail; }

    /* 输入类型：NV12 渐进 */
    IMFMediaType *in = NULL;
    if (MFCreateMediaType(&in) != S_OK) goto fail;
    in->lpVtbl->SetGUID((IMFAttributes *)in, &MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    in->lpVtbl->SetGUID((IMFAttributes *)in, &MF_MT_SUBTYPE, &MFVideoFormat_NV12);
    MFSetAttributeSize(in, MF_MT_FRAME_SIZE, (UINT32)e->w, (UINT32)e->h);
    MFSetAttributeRatio(in, MF_MT_FRAME_RATE, 30, 1);
    in->lpVtbl->SetUINT32((IMFAttributes *)in, &MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    in->lpVtbl->SetUINT32((IMFAttributes *)in, &MF_MT_ALL_SAMPLES_INDEPENDENT, 0);
    hr = e->mft->lpVtbl->SetInputType(e->mft, 0, in, 0);
    in->lpVtbl->Release(in);
    if (hr != S_OK) { DBG("SetInputType(NV12) 失败"); goto fail; }

    /* 流启动 + 可选 ICodecAPI（强制关键帧用） */
    e->mft->lpVtbl->ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    e->mft->lpVtbl->ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    e->mft->lpVtbl->QueryInterface(e->mft, &IID_ICodecAPI, (void **)&e->codec);

    DBG("MFT 就绪 %dx%d", e->w, e->h);
    return e;
#endif

fail:
    yuduan_enc_destroy(e);
    return NULL;
}

uint32_t yuduan_enc_encode(void *handle, const uint8_t *rgba, uint32_t rgba_len,
                           uint8_t *out, uint32_t out_cap) {
    mf_enc *e = handle;
    if (!e || e->failed || !rgba || rgba_len < (uint32_t)(e->w * e->h * 4) || !out || !out_cap)
        return 0;

    if (e->out_pending) {
        if (e->out.n > out_cap) return 0;
        memcpy(out, e->out.b, e->out.n);
        e->out_pending = 0;
        return (uint32_t)e->out.n;
    }

#ifndef _WIN32
    (void)e; (void)rgba;
    return 0;
#else
    /* 1) NV12 暂存 -> 输入 sample */
    rgba_to_nv12(rgba, e->w, e->h, e->nv12, e->w,
                 e->nv12 + (size_t)e->w * e->h, e->w);
    IMFMediaBuffer *ib = NULL;
    IMFSample *sample = NULL;
    if (MFCreateMemoryBuffer((UINT32)((size_t)e->w * e->h * 3 / 2), &ib) != S_OK) goto err;
    BYTE *dst = NULL;
    if (ib->lpVtbl->Lock(ib, &dst, NULL, NULL) != S_OK) goto err;
    memcpy(dst, e->nv12, (size_t)e->w * e->h * 3 / 2);
    ib->lpVtbl->SetCurrentLength(ib, (DWORD)((size_t)e->w * e->h * 3 / 2));
    ib->lpVtbl->Unlock(ib);
    if (MFCreateSample(&sample) != S_OK) goto err;
    sample->lpVtbl->AddBuffer(sample, ib);
    sample->lpVtbl->SetSampleTime(sample, e->frame * 333333); /* 30fps，100ns 单位 */
    sample->lpVtbl->SetSampleDuration(sample, 333333);

    /* 2) GOP 关键帧：ICodecAPI 强推（首帧必为关键帧） */
    if (e->codec && (e->frame == 0 || (e->gop > 0 && e->frame % e->gop == 0))) {
        PROPVARIANT pv; memset(&pv, 0, sizeof(pv));
        pv.vt = VT_UI4; pv.u.ul = 1;
        e->codec->lpVtbl->SetValue(e->codec, &CODECAPI_AVEncVideoForceKeyFrame, &pv);
    }

    /* 3) 提交输入，拉取输出（MFT 可能缓存若干输入后才出第一帧：NEED_MORE_INPUT 正常） */
    HRESULT hr = e->mft->lpVtbl->ProcessInput(e->mft, 0, sample, 0);
    sample->lpVtbl->Release(sample);
    ib->lpVtbl->Release(ib);
    if (hr != S_OK) { DBG("ProcessInput 失败 frame=%lld", (long long)e->frame); goto err; }
    e->frame++;

    MFT_OUTPUT_STREAM_INFO si; memset(&si, 0, sizeof(si));
    MFT_OUTPUT_DATA_BUFFER odb; memset(&odb, 0, sizeof(odb));
    odb.dwStreamID = 0;
    IMFSample *os = NULL;
    IMFMediaBuffer *ob = NULL;
    if (MFCreateMemoryBuffer((UINT32)((size_t)e->w * e->h + (1 << 16)), &ob) == S_OK &&
        MFCreateSample(&os) == S_OK) {
        os->lpVtbl->AddBuffer(os, ob);
        odb.pSample = os;
        hr = e->mft->lpVtbl->ProcessOutput(e->mft, 0, 1, &odb, &(UINT32){0});
        if (hr == S_OK && odb.pSample) {
            if (sample_to_out(e, odb.pSample) == 0 && e->out.n > 0) {
                e->out_pending = 1;
                DBG("frame=%lld out=%zu bytes", (long long)e->frame - 1, e->out.n);
            }
        } else if (hr != MF_E_TRANSFORM_NEED_MORE_INPUT) {
            DBG("ProcessOutput 失败 0x%08lx", (unsigned long)hr);
        }
        if (odb.pSample) odb.pSample->lpVtbl->Release(odb.pSample);
        if (ob) ob->lpVtbl->Release(ob);
    }

    if (e->out_pending) {
        if (e->out.n > out_cap) return 0;
        memcpy(out, e->out.b, e->out.n);
        e->out_pending = 0;
        return (uint32_t)e->out.n;
    }
    /* MFT 管线未出帧（NEED_MORE_INPUT）：返回 0 且无 pending —— 副端语义：
       encode 返回 0 且 out 未产出会被当作「缓冲不足重试」，为避免死循环，
       这里返回一个极小的合法 SPS-only 占位？—— 不。契约：0=缓冲不足。
       副端 rc.rs 对「输出 0 字节」按丢弃重采处理，可接受（下一帧继续拉）。 */
    return 0;

err:
    if (sample) sample->lpVtbl->Release(sample);
    if (ib) ib->lpVtbl->Release(ib);
    e->failed = 1;
    return 0;
#endif
}

void yuduan_enc_destroy(void *handle) {
    mf_enc *e = handle;
    if (!e) return;
#ifdef _WIN32
    if (e->mft) {
        e->mft->lpVtbl->ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        e->mft->lpVtbl->ProcessMessage(e->mft, MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        e->mft->lpVtbl->Release(e->mft);
    }
    if (e->codec) e->codec->lpVtbl->Release(e->codec);
    if (e->mf_started) MFShutdown();
    if (e->com_init) CoUninitialize();
#endif
    free(e->nv12);
    mf_free_out(e);
    free(e);
}
