/*
 * yuduan_enc_vt.c — 御端原生编码器（macOS VideoToolbox 后端）
 *
 * 实现 protocol.md §8.3.3 C-ABI 契约：
 *   void*    yuduan_enc_create (const char* codec, uint32_t width, uint32_t height);
 *   uint32_t yuduan_enc_encode(void* h, const uint8_t* rgba, uint32_t rgba_len,
 *                              uint8_t* out, uint32_t out_cap);
 *   void     yuduan_enc_destroy(void* h);
 *
 * 行为：
 *   - 仅接受 "h264" / "h265"（其余 codec 返回 NULL，供能力探测不虚标）；
 *   - 输入 RGBA8（紧凑，w*h*4 字节），内部转 BGRA 喂 VideoToolbox；
 *   - 输出 Annex-B（00 00 00 01 起始码）；参数集（H.264: SPS/PPS；HEVC: VPS/SPS/PPS）
 *     在 create() 预热阶段捕获，随首个真实输出前置；首个真实输出强制 IDR；
 *   - 返回写入字节数；out 不足返回 0（调用方扩张重试）。
 *
 * 回调时序（实测关键点）：
 *   部分机器 HW H.264 编码器冷启动极慢（首帧回调可 >30s），故 create() 内
 *   编码一个黑帧做预热并捕获参数集——一次性吸收冷启动，之后真实帧回调即时；
 *   每次编码经 sourceFrameRefCon 携带请求代次，迟到回调按代次丢弃，不污染当前帧。
 *
 * 构建（见 build.sh）：
 *   clang -dynamiclib -O2 -framework VideoToolbox -framework CoreMedia \
 *         -framework CoreFoundation -framework CoreVideo yuduan_enc_vt.c -o libyuduan_enc_vt.dylib
 */

#include <VideoToolbox/VideoToolbox.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CoreVideo.h>
#include <dispatch/dispatch.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define VT_WAIT_SEC 30   /* 单帧回调等待上限（含预热帧） */

/* 诊断输出：YUDUAN_ENC_DEBUG=1 时打印会话创建/编码各阶段 OSStatus 与耗时。
 * 生产副端不必开启；排障（新机器/新系统版本编码器不可用）时可据此定位。 */
static int vt_debug(void) {
    static int init = 0, on = 0;
    if (!init) {
        const char *e = getenv("YUDUAN_ENC_DEBUG");
        on = (e && (strcmp(e, "1") == 0 || strcasecmp(e, "true") == 0)) ? 1 : 0;
        init = 1;
    }
    return on;
}
#define DBG(...) do { if (vt_debug()) fprintf(stderr, "[enc_vt] " __VA_ARGS__); } while (0)

void yuduan_enc_destroy(void *handle); /* 前置声明（create 失败清理用） */

typedef struct {
    VTCompressionSessionRef session;
    dispatch_semaphore_t sem;     /* 编码完成信号（同步编码） */
    uint8_t *buf;                 /* 当前帧 Annex-B 输出缓冲 */
    size_t cap, len;
    uint8_t *ps_buf;              /* 预热捕获的参数集（含起始码） */
    size_t ps_len;
    int ps_sent;                  /* 参数集是否已随首个真实输出前置 */
    int warmup;                   /* 1=预热阶段（回调只捕获参数集，不出 NAL） */
    int w, h;
    long frame;                   /* 帧序号（时间戳用） */
    CMVideoCodecType ctype;       /* kCMVideoCodecType_H264 / HEVC */
    int failed;
    /* 回调配对：VTCompressionSessionEncodeFrame 经 sourceFrameRefCon 携带请求代次，
     * 回调返回时带回——迟到/过期的回调据此丢弃，不污染当前帧输出。 */
    unsigned long long req_gen;
    volatile unsigned long long done_gen;
} vt_enc;

static const uint8_t SC4[4] = {0x00, 0x00, 0x00, 0x01};

/* 追加字节；内存不足返回 -1 */
static int buf_append(uint8_t **buf, size_t *cap, size_t *len, const uint8_t *p, size_t n) {
    if (*len + n > *cap) {
        size_t ncap = *cap ? *cap * 2 : 1 << 16;
        while (ncap < *len + n) ncap *= 2;
        uint8_t *nb = realloc(*buf, ncap);
        if (!nb) return -1;
        *buf = nb;
        *cap = ncap;
    }
    memcpy(*buf + *len, p, n);
    *len += n;
    return 0;
}

/* 从格式描述提取参数集（H.264: SPS+PPS；HEVC: VPS/SPS/PPS），Annex-B 前缀后追加 */
static void emit_parameter_sets(uint8_t **buf, size_t *cap, size_t *len, CMVideoCodecType ctype,
                                CMFormatDescriptionRef desc) {
    const uint8_t *ps[3];
    size_t pslen[3];
    size_t cnt = 0;
    if (ctype == kCMVideoCodecType_H264) {
        for (int i = 0; i < 2 && cnt < 3; i++) {
            if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    desc, i, &ps[cnt], &pslen[cnt], NULL, NULL) == noErr) {
                cnt++;
            }
        }
    } else {
        for (int i = 0; i < 3 && cnt < 3; i++) {
            if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    desc, i, &ps[cnt], &pslen[cnt], NULL, NULL) == noErr) {
                cnt++;
            }
        }
    }
    for (size_t i = 0; i < cnt; i++) {
        buf_append(buf, cap, len, SC4, 4);
        buf_append(buf, cap, len, ps[i], pslen[i]);
    }
}

/* AVCC（4 字节长度前缀 NALU 序列）→ Annex-B */
static void emit_avcc_as_annexb(uint8_t **buf, size_t *cap, size_t *len,
                                const uint8_t *data, size_t n) {
    size_t off = 0;
    while (off + 4 <= n) {
        uint32_t sz = ((uint32_t)data[off] << 24) | ((uint32_t)data[off + 1] << 16) |
                      ((uint32_t)data[off + 2] << 8) | (uint32_t)data[off + 3];
        off += 4;
        if (sz == 0 || off + sz > n) break;
        buf_append(buf, cap, len, SC4, 4);
        buf_append(buf, cap, len, data + off, sz);
        off += sz;
    }
}

static void compression_cb(void *ctx, void *source, OSStatus status,
                           VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    vt_enc *vt = (vt_enc *)ctx;
    (void)flags;
    unsigned long long g = (unsigned long long)(uintptr_t)source;
    if (status != noErr || !sb || !CMSampleBufferDataIsReady(sb)) {
        vt->failed = 1;
        vt->done_gen = g;
        dispatch_semaphore_signal(vt->sem);
        return;
    }
    if (g != vt->req_gen) {
        /* 迟到/过期回调（预热/超时后抵达）：丢弃，不污染当前帧输出 */
        vt->done_gen = g;
        dispatch_semaphore_signal(vt->sem);
        return;
    }
    CMFormatDescriptionRef desc = CMSampleBufferGetFormatDescription(sb);
    if (vt->warmup) {
        /* 预热帧：仅捕获参数集，不出 NAL（输出被丢弃） */
        if (desc && !vt->ps_buf) {
            uint8_t *psb = NULL;
            size_t psc = 0, psl = 0;
            emit_parameter_sets(&psb, &psc, &psl, vt->ctype, desc);
            if (psl > 0) {
                vt->ps_buf = psb;
                vt->ps_len = psl;
            } else if (psb) {
                free(psb);
            }
        }
    } else {
        CMBlockBufferRef bb = CMSampleBufferGetDataBuffer(sb);
        size_t total = CMBlockBufferGetDataLength(bb);
        if (total > 0) {
            uint8_t *tmp = malloc(total);
            if (tmp) {
                if (CMBlockBufferCopyDataBytes(bb, 0, total, tmp) == kCMBlockBufferNoErr) {
                    if (!vt->ps_sent && vt->ps_buf) {
                        /* 参数集随首个真实输出前置（该输出强制为 IDR，可独立解码） */
                        buf_append(&vt->buf, &vt->cap, &vt->len, vt->ps_buf, vt->ps_len);
                        vt->ps_sent = 1;
                    }
                    emit_avcc_as_annexb(&vt->buf, &vt->cap, &vt->len, tmp, total);
                }
                free(tmp);
            }
        }
    }
    vt->done_gen = g;
    dispatch_semaphore_signal(vt->sem);
}

static void release_pixel_cb(void *releaseRefCon, const void *baseAddress) {
    (void)releaseRefCon;
    free((void *)baseAddress);
}

/* 编码一帧（RGBA→BGRA 复制；调用方已持锁串行），等待本代次回调。
 * 返回 0=失败/超时；*buf_len 返回输出长度。 */
static int encode_frame_wait(vt_enc *vt, const uint8_t *rgba, int force_kf, long ts) {
    size_t need = (size_t)vt->w * vt->h * 4;
    uint8_t *bgra = malloc(need);
    if (!bgra) return -1;
    if (rgba) {
        for (size_t i = 0; i < need; i += 4) {
            bgra[i] = rgba[i + 2];
            bgra[i + 1] = rgba[i + 1];
            bgra[i + 2] = rgba[i];
            bgra[i + 3] = rgba[i + 3];
        }
    } else {
        memset(bgra, 0, need); /* 预热黑帧 */
    }

    CVImageBufferRef pb = NULL;
    CVReturn cvr = CVPixelBufferCreateWithBytes(
        NULL, vt->w, vt->h, kCVPixelFormatType_32BGRA, bgra, (size_t)vt->w * 4,
        release_pixel_cb, NULL, NULL, &pb);
    if (cvr != kCVReturnSuccess) {
        free(bgra);
        DBG("CVPixelBufferCreateWithBytes 失败 cvr=%d\n", (int)cvr);
        return -1;
    }

    vt->len = 0;
    vt->failed = 0;

    CMTime pts = CMTimeMake(ts, 30);
    CMTime dur = CMTimeMake(1, 30);
    CFMutableDictionaryRef fprops = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFCopyStringDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks);
    if (force_kf) {
        CFDictionarySetValue(fprops, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
    }
    unsigned long long gen = ++vt->req_gen;
    OSStatus st = VTCompressionSessionEncodeFrame(
        vt->session, pb, pts, dur, fprops, (void *)(uintptr_t)gen, NULL);
    CFRelease(fprops);
    CVBufferRelease(pb); /* bgra 由 release_pixel_cb 在像素缓冲释放时 free */
    if (st != noErr) {
        DBG("VTCompressionSessionEncodeFrame 失败 st=%d\n", (int)st);
        return -1;
    }
    /* 轮询等待本代次回调（迟到回调会信号但代次不匹配，继续等本轮） */
    dispatch_time_t deadline = dispatch_time(DISPATCH_TIME_NOW, (int64_t)VT_WAIT_SEC * NSEC_PER_SEC);
    while (vt->done_gen != gen) {
        (void)dispatch_semaphore_wait(vt->sem, dispatch_time(DISPATCH_TIME_NOW, (int64_t)200 * NSEC_PER_MSEC));
        if (dispatch_time(DISPATCH_TIME_NOW, 0) >= deadline && vt->done_gen != gen) {
            DBG("等待第 %ld 帧回调超时(%ds)\n", (long)ts, VT_WAIT_SEC);
            return -1; /* 超时：本帧丢弃；迟到回调将因代次不匹配被丢弃 */
        }
    }
    DBG("帧 ts=%ld 回调 gen=%llu len=%zu failed=%d\n", (long)ts, vt->done_gen, vt->len, vt->failed);
    return vt->failed ? -1 : 0;
}

void *yuduan_enc_create(const char *codec, uint32_t width, uint32_t height) {
    CMVideoCodecType ctype;
    if (strcmp(codec, "h264") == 0) {
        ctype = kCMVideoCodecType_H264;
    } else if (strcmp(codec, "h265") == 0) {
        ctype = kCMVideoCodecType_HEVC;
    } else {
        return NULL; /* 供能力探测不虚标 */
    }
    if (width == 0 || height == 0 || (width % 2) || (height % 2)) {
        return NULL;
    }

    vt_enc *vt = calloc(1, sizeof(vt_enc));
    if (!vt) return NULL;
    vt->w = (int)width;
    vt->h = (int)height;
    vt->ctype = ctype;
    vt->sem = dispatch_semaphore_create(0);
    if (!vt->sem) {
        free(vt);
        return NULL;
    }

    /* sourceImageBufferAttributes：像素缓冲尺寸提示 */
    int32_t cvW = (int32_t)width, cvH = (int32_t)height;
    CFMutableDictionaryRef pbAttrs = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFCopyStringDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks);
    CFNumberRef nW = CFNumberCreate(NULL, kCFNumberSInt32Type, &cvW);
    CFNumberRef nH = CFNumberCreate(NULL, kCFNumberSInt32Type, &cvH);
    CFDictionarySetValue(pbAttrs, kCVPixelBufferWidthKey, nW);
    CFDictionarySetValue(pbAttrs, kCVPixelBufferHeightKey, nH);

    /* 编码器选择（YUDUAN_ENC_HW）：
     *   auto/未设置 = 不指定（交给 VideoToolbox 启发式，实测 H.264/HEVC 均可编）；
     *   0           = 强制软件编码器（排查硬件编码器异常时用；实测本机软件 H.264
     *                 会话回调不返回，故**不可作为默认路径**）；
     *   1           = 强制硬件编码器（kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder）。
     * 默认 auto 的理由：铁律 2 要求三平台同构，编码器选型不应绑定单一后端；
     * 由系统启发式决定在 Apple Silicon/Intel 各机型上都能拿到可用编码器。 */
    const char *hwenv = getenv("YUDUAN_ENC_HW");
    CFMutableDictionaryRef encSpec = NULL;
    if (hwenv && (strcmp(hwenv, "0") == 0 || strcmp(hwenv, "1") == 0)) {
        encSpec = CFDictionaryCreateMutable(
            kCFAllocatorDefault, 0, &kCFCopyStringDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks);
        CFStringRef key = (strcmp(hwenv, "1") == 0)
            ? kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder
            : kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder;
        CFDictionarySetValue(encSpec, key,
                             (strcmp(hwenv, "1") == 0) ? kCFBooleanTrue : kCFBooleanFalse);
        DBG("编码器规格: %s\n", strcmp(hwenv, "1") == 0 ? "强制硬件" : "强制软件");
    }

    OSStatus st = VTCompressionSessionCreate(
        kCFAllocatorDefault, (int32_t)width, (int32_t)height, ctype,
        encSpec, pbAttrs, NULL,
        compression_cb, vt, &vt->session);
    if (encSpec) CFRelease(encSpec);
    CFRelease(nW);
    CFRelease(nH);
    CFRelease(pbAttrs);
    if (st != noErr || !vt->session) {
        DBG("VTCompressionSessionCreate 失败 st=%d\n", (int)st);
        if (vt->sem) dispatch_release(vt->sem);
        free(vt);
        return NULL;
    }

    VTSessionSetProperty(vt->session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(vt->session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    CFNumberRef maxKF = CFNumberCreate(NULL, kCFNumberIntType, &(int){60});
    VTSessionSetProperty(vt->session, kVTCompressionPropertyKey_MaxKeyFrameInterval, maxKF);
    CFRelease(maxKF);
    if (ctype == kCMVideoCodecType_H264) {
        VTSessionSetProperty(vt->session, kVTCompressionPropertyKey_ProfileLevel,
                             kVTProfileLevel_H264_Main_AutoLevel);
    } else {
        VTSessionSetProperty(vt->session, kVTCompressionPropertyKey_ProfileLevel,
                             kVTProfileLevel_HEVC_Main_AutoLevel);
    }
    if (VTCompressionSessionPrepareToEncodeFrames(vt->session) != noErr) {
        VTCompressionSessionInvalidate(vt->session);
        CFRelease(vt->session);
        dispatch_release(vt->sem);
        free(vt);
        return NULL;
    }

    /* 预热：编码一帧黑帧，吸收硬件编码器冷启动并捕获参数集。
     * 失败/超时 → 会话不可信，返回 NULL（副端诚实回退 RAW_RGBA）。 */
    vt->warmup = 1;
    int wok = encode_frame_wait(vt, NULL, 1, 0) == 0 && vt->ps_buf != NULL;
    vt->warmup = 0;
    if (!wok) {
        DBG("预热失败：ps_buf=%p（编码器不可信，诚实失败→副端回退）\n", (void *)vt->ps_buf);
        yuduan_enc_destroy(vt);
        return NULL;
    }
    return vt;
}

uint32_t yuduan_enc_encode(void *handle, const uint8_t *rgba, uint32_t rgba_len,
                           uint8_t *out, uint32_t out_cap) {
    vt_enc *vt = (vt_enc *)handle;
    if (!vt || !rgba || !out) return 0;
    size_t need = (size_t)vt->w * vt->h * 4;
    if ((size_t)rgba_len < need) return 0; /* 帧与会话尺寸不符：交由上层回退 */

    int rc = encode_frame_wait(vt, rgba, !vt->ps_sent, vt->frame);
    vt->frame++;
    if (rc != 0 || vt->len == 0) return 0;
    if ((size_t)out_cap < vt->len) return 0; /* 调用方扩张重试 */
    memcpy(out, vt->buf, vt->len);
    return (uint32_t)vt->len;
}

void yuduan_enc_destroy(void *handle) {
    vt_enc *vt = (vt_enc *)handle;
    if (!vt) return;
    if (vt->session) {
        VTCompressionSessionInvalidate(vt->session);
        CFRelease(vt->session);
    }
    if (vt->sem) dispatch_release(vt->sem);
    free(vt->buf);
    free(vt->ps_buf);
    free(vt);
}
