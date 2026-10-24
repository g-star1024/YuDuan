/*
 * yuduan_dec_vt.c — 御端主端原生解码器（macOS VideoToolbox 后端）
 *
 * 实现 protocol.md §8.3.18 C-ABI 契约（与副端编码器 §8.3.3 对称）：
 *   void*    yuduan_dec_create(const char* codec, uint32_t width, uint32_t height);
 *   uint32_t yuduan_dec_decode(void* h, const uint8_t* data, uint32_t len,
 *                              uint8_t* out, uint32_t out_cap);
 *   uint32_t yuduan_dec_out_size(void* h);
 *   void     yuduan_dec_destroy(void* h);
 *
 * 行为：
 *   - 仅接受 "h264" / "h265"（其余 codec 返回 NULL，供能力探测不虚标——
 *     与副端 NativeEncoder::try_load 同口径）；
 *   - 输入 Annex-B（00 00 00 01 / 00 00 01 起始码），与副端 VT 编码器输出一致；
 *     参数集（H.264: SPS/PPS；HEVC: VPS/SPS/PPS）从码流中提取，用于建
 *     CMVideoFormatDescription（解码器可随时起播，不依赖带外协商）；
 *   - 输出 RGBA8（紧凑 w*h*4）；返回写入字节数，0 = 本帧未产出图像
 *     （未拿到参数集 / 解码失败 / out 不足）。调用方按「等下一个关键帧」处理，
 *     **不得**以填充黑帧冒充解码成功（铁律 6：禁止空口/造假）；
 *   - 同步解码（不设 kVTDecodeFrame_EnableAsynchronousDecompression），
 *     回调在 VTDecompressionSessionDecodeFrame 返回前完成——主端单线程收帧循环
 *     无需额外同步，也不引入输出乱序。
 *
 * 像素格式策略：优先向 VT 请求 32BGRA 直出（一次 BGRA→RGBA 重排即可上屏）；
 * 若会话创建失败或首帧未出图，降级 420v（NV12）并做 YUV→RGBA 转换；
 * 两者皆不可用时诚实返回 0（主端回退「帧统计模式」）。
 *
 * 构建（见 build.sh）：
 *   clang -dynamiclib -O2 -framework VideoToolbox -framework CoreMedia \
 *         -framework CoreFoundation -framework CoreVideo yuduan_dec_vt.c \
 *         -o libyuduan_dec_vt.dylib
 *
 * 环境变量：
 *   YUDUAN_DEC_DEBUG=1  打印会话创建/参数集/出帧各阶段 OSStatus（排障用）
 *   YUDUAN_DEC_PF=bgra|nv12  强制输出像素格式（默认 bgra 优先、失败降级 nv12）
 */

#include <VideoToolbox/VideoToolbox.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CoreVideo.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define MAX_NAL 64

/* 前向声明：输出回调（定义在下方，会话创建处引用） */
static void dec_cb(void *refcon, void *sourceFrameRefCon, OSStatus status, VTDecodeInfoFlags flags,
                   CVImageBufferRef image, CMTime pts, CMTime dur);

static int dec_debug(void) {
    static int init = 0, on = 0;
    if (!init) {
        const char *e = getenv("YUDUAN_DEC_DEBUG");
        on = (e && (strcmp(e, "1") == 0 || strcasecmp(e, "true") == 0)) ? 1 : 0;
        init = 1;
    }
    return on;
}

typedef struct {
    const uint8_t *p;
    size_t n;
} Nal;

typedef struct {
    int is_hevc;
    uint32_t width, height;

    CMFormatDescriptionRef fmt;
    VTDecompressionSessionRef sess;
    int pixel_format; /* 0=未知 1=BGRA 2=NV12 */

    uint8_t *sps;
    size_t sps_len;
    uint8_t *pps;
    size_t pps_len;
    uint8_t *vps;
    size_t vps_len;

    /* 输出暂存（回调写入，decode() 拷给调用方） */
    uint8_t *out;
    uint32_t out_cap;
    uint32_t out_written;
    uint32_t out_w, out_h;
    int have_out;

    uint64_t frames_in;
    uint64_t frames_out;
    int last_status;
} Dec;

/* —— Annex-B 切分（与副端/主端共享黄金向量口径：internal/annexb） —— */
static int split_annexb(const uint8_t *d, size_t len, Nal *out, int max) {
    int cnt = 0;
    size_t i = 0, start = 0;
    while (i + 2 < len) {
        if (d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) {
            i += 3;
            break;
        }
        if (i + 3 < len && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1) {
            i += 4;
            break;
        }
        i++;
    }
    if (i >= len) return 0;
    start = i;
    while (i < len) {
        if (i + 2 < len && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1) {
            if (cnt < max) {
                out[cnt].p = d + start;
                out[cnt].n = i - start;
                cnt++;
            }
            i += 3;
            start = i;
        } else if (i + 3 < len && d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 0 && d[i + 3] == 1) {
            if (cnt < max) {
                out[cnt].p = d + start;
                out[cnt].n = i - start;
                cnt++;
            }
            i += 4;
            start = i;
        } else {
            i++;
        }
    }
    if (start < len && cnt < max) {
        out[cnt].p = d + start;
        out[cnt].n = len - start;
        cnt++;
    }
    return cnt;
}

static void store_param(uint8_t **dst, size_t *dst_len, const uint8_t *p, size_t n) {
    if (*dst && *dst_len == n && memcmp(*dst, p, n) == 0) return; /* 未变，不重建会话 */
    free(*dst);
    *dst = (uint8_t *)malloc(n ? n : 1);
    memcpy(*dst, p, n);
    *dst_len = n;
}

static void teardown_session(Dec *d) {
    if (d->sess) {
        VTDecompressionSessionInvalidate(d->sess);
        CFRelease(d->sess);
        d->sess = NULL;
    }
    if (d->fmt) {
        CFRelease(d->fmt);
        d->fmt = NULL;
    }
    d->pixel_format = 0;
}

static int build_format(Dec *d) {
    OSStatus st = noErr;
    if (!d->is_hevc) {
        if (!d->sps || !d->pps) return -1;
        const uint8_t *params[2] = {d->sps, d->pps};
        size_t sizes[2] = {d->sps_len, d->pps_len};
        st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
            kCFAllocatorDefault, 2, params, sizes, 4, &d->fmt);
    } else {
        if (!d->vps || !d->sps || !d->pps) return -1;
        const uint8_t *params[3] = {d->vps, d->sps, d->pps};
        size_t sizes[3] = {d->vps_len, d->sps_len, d->pps_len};
        st = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            kCFAllocatorDefault, 3, params, sizes, 4, NULL, &d->fmt);
    }
    if (st != noErr || d->fmt == NULL) {
        if (dec_debug()) fprintf(stderr, "[dec] FormatDescription 创建失败 st=%d\n", (int)st);
        return -1;
    }
    return 0;
}

static int create_session(Dec *d, int force_pf) {
    CFMutableDictionaryRef dest = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (!dest) return -1;

    int order[2];
    if (force_pf == 2) {
        order[0] = 2;
        order[1] = 1;
    } else {
        order[0] = 1;
        order[1] = 2;
    }
    for (int k = 0; k < 2; k++) {
        OSType pf = (order[k] == 1) ? kCVPixelFormatType_32BGRA
                                    : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        CFNumberRef n = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pf);
        CFDictionarySetValue(dest, kCVPixelBufferPixelFormatTypeKey, n);
        CFRelease(n);

        VTDecompressionOutputCallbackRecord cb;
        cb.decompressionOutputCallback = dec_cb; /* 前向声明见文件顶部 */
        cb.decompressionOutputRefCon = d;

        OSStatus st = VTDecompressionSessionCreate(kCFAllocatorDefault, d->fmt, NULL, dest, &cb,
                                                   &d->sess);
        if (st == noErr && d->sess) {
            d->pixel_format = order[k];
            if (dec_debug())
                fprintf(stderr, "[dec] 会话创建成功 pf=%s\n", order[k] == 1 ? "BGRA" : "NV12");
            CFRelease(dest);
            return 0;
        }
        if (dec_debug())
            fprintf(stderr, "[dec] 会话创建失败 pf=%s st=%d\n", order[k] == 1 ? "BGRA" : "NV12", (int)st);
        d->sess = NULL;
    }
    CFRelease(dest);
    return -1;
}

/* —— 输出回调 —— */
static void nv12_to_rgba(const uint8_t *y, size_t y_stride, const uint8_t *uv, size_t uv_stride,
                         size_t w, size_t h, uint8_t *out) {
    for (size_t r = 0; r < h; r++) {
        const uint8_t *yr = y + r * y_stride;
        const uint8_t *uvr = uv + (r / 2) * uv_stride;
        uint8_t *o = out + r * w * 4;
        for (size_t c = 0; c < w; c++) {
            int Y = yr[c];
            int U = uvr[(c / 2) * 2 + 0];
            int V = uvr[(c / 2) * 2 + 1];
            int yv = Y - 16;
            int rv = (298 * yv + 409 * (V - 128) + 128) >> 8;
            int gv = (298 * yv - 100 * (U - 128) - 208 * (V - 128) + 128) >> 8;
            int bv = (298 * yv + 516 * (U - 128) + 128) >> 8;
            o[c * 4 + 0] = (uint8_t)(rv < 0 ? 0 : (rv > 255 ? 255 : rv));
            o[c * 4 + 1] = (uint8_t)(gv < 0 ? 0 : (gv > 255 ? 255 : gv));
            o[c * 4 + 2] = (uint8_t)(bv < 0 ? 0 : (bv > 255 ? 255 : bv));
            o[c * 4 + 3] = 255;
        }
    }
}

static void dec_cb(void *refcon, void *sourceFrameRefCon, OSStatus status, VTDecodeInfoFlags flags,
                   CVImageBufferRef image, CMTime pts, CMTime dur) {
    (void)sourceFrameRefCon;
    (void)flags;
    (void)pts;
    (void)dur;
    Dec *d = (Dec *)refcon;
    if (!d) return;
    if (status != noErr) {
        d->last_status = (int)status;
        if (dec_debug()) fprintf(stderr, "[dec] 回调 status=%d\n", (int)status);
        return;
    }
    if (!image) return;
    if (CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return;

    size_t w = CVPixelBufferGetWidth(image);
    size_t h = CVPixelBufferGetHeight(image);
    uint32_t need = (uint32_t)(w * h * 4);
    if (d->out && d->out_cap >= need) {
        OSType pf = CVPixelBufferGetPixelFormatType(image);
        if (pf == kCVPixelFormatType_32BGRA) {
            size_t stride = CVPixelBufferGetBytesPerRow(image);
            const uint8_t *src = (const uint8_t *)CVPixelBufferGetBaseAddress(image);
            for (size_t r = 0; r < h; r++) {
                const uint8_t *s = src + r * stride;
                uint8_t *o = d->out + r * w * 4;
                for (size_t c = 0; c < w; c++) {
                    o[c * 4 + 0] = s[c * 4 + 2]; /* R ← B */
                    o[c * 4 + 1] = s[c * 4 + 1]; /* G */
                    o[c * 4 + 2] = s[c * 4 + 0]; /* B ← R */
                    o[c * 4 + 3] = 255;
                }
            }
            d->out_written = need;
            d->out_w = (uint32_t)w;
            d->out_h = (uint32_t)h;
            d->have_out = 1;
            d->frames_out++;
        } else if (pf == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
                   pf == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) {
            const uint8_t *yp = (const uint8_t *)CVPixelBufferGetBaseAddressOfPlane(image, 0);
            const uint8_t *uvp = (const uint8_t *)CVPixelBufferGetBaseAddressOfPlane(image, 1);
            size_t ys = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
            size_t uvs = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
            if (yp && uvp) {
                nv12_to_rgba(yp, ys, uvp, uvs, w, h, d->out);
                d->out_written = need;
                d->out_w = (uint32_t)w;
                d->out_h = (uint32_t)h;
                d->have_out = 1;
                d->frames_out++;
            }
        } else {
            d->last_status = -2; /* 未支持的像素格式：诚实报 0，不造假帧 */
        }
    }
    CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
}

/* —— C-ABI —— */
void *yuduan_dec_create(const char *codec, uint32_t width, uint32_t height) {
    int is_hevc = 0;
    if (!codec) return NULL;
    if (strcmp(codec, "h264") == 0) {
        is_hevc = 0;
    } else if (strcmp(codec, "h265") == 0 || strcmp(codec, "hevc") == 0) {
        is_hevc = 1;
    } else {
        return NULL; /* 不虚标：不支持的 codec 直接拒绝 */
    }
    if (width == 0 || height == 0) return NULL;

    Dec *d = (Dec *)calloc(1, sizeof(Dec));
    if (!d) return NULL;
    d->is_hevc = is_hevc;
    d->width = width;
    d->height = height;
    const char *pf = getenv("YUDUAN_DEC_PF");
    if (pf && strcmp(pf, "nv12") == 0) d->pixel_format = 2;
    if (pf && strcmp(pf, "bgra") == 0) d->pixel_format = 1;
    if (dec_debug())
        fprintf(stderr, "[dec] create codec=%s %ux%u\n", is_hevc ? "h265" : "h264", width, height);
    return d;
}

uint32_t yuduan_dec_out_size(void *h) {
    Dec *d = (Dec *)h;
    if (!d) return 0;
    return d->width * d->height * 4;
}

uint32_t yuduan_dec_decode(void *h, const uint8_t *data, uint32_t len, uint8_t *out,
                           uint32_t out_cap) {
    Dec *d = (Dec *)h;
    if (!d || !data || len == 0 || !out || out_cap == 0) return 0;

    Nal nals[MAX_NAL];
    int n = split_annexb(data, len, nals, MAX_NAL);
    if (n <= 0) return 0;

    /* 1) 抽取参数集（H.264: 7=SPS 8=PPS；HEVC: 32=VPS 33=SPS 34=PPS） */
    int got_param = 0;
    for (int i = 0; i < n; i++) {
        if (nals[i].n < 1) continue;
        uint8_t b = nals[i].p[0];
        if (!d->is_hevc) {
            int t = b & 0x1F;
            if (t == 7) {
                store_param(&d->sps, &d->sps_len, nals[i].p, nals[i].n);
                got_param = 1;
            } else if (t == 8) {
                store_param(&d->pps, &d->pps_len, nals[i].p, nals[i].n);
                got_param = 1;
            }
        } else {
            int t = (b >> 1) & 0x3F;
            if (t == 32) {
                store_param(&d->vps, &d->vps_len, nals[i].p, nals[i].n);
                got_param = 1;
            } else if (t == 33) {
                store_param(&d->sps, &d->sps_len, nals[i].p, nals[i].n);
                got_param = 1;
            } else if (t == 34) {
                store_param(&d->pps, &d->pps_len, nals[i].p, nals[i].n);
                got_param = 1;
            }
        }
    }
    if (got_param) {
        teardown_session(d);
        if (build_format(d) == 0) create_session(d, d->pixel_format);
    }
    if (!d->sess || !d->fmt) return 0; /* 还没拿到参数集：等下一个关键帧 */

    /* 2) VCL NAL → AVCC（4 字节大端长度前缀） */
    size_t total = 0;
    int vcl = 0;
    for (int i = 0; i < n; i++) {
        if (nals[i].n < 1) continue;
        uint8_t b = nals[i].p[0];
        int is_param;
        if (!d->is_hevc) {
            int t = b & 0x1F;
            is_param = (t == 7 || t == 8 || t == 9 || t == 6);
        } else {
            int t = (b >> 1) & 0x3F;
            is_param = (t == 32 || t == 33 || t == 34 || t == 35 || t == 39);
        }
        if (is_param) continue;
        total += 4 + nals[i].n;
        vcl++;
    }
    if (!vcl) return 0;

    uint8_t *avcc = (uint8_t *)malloc(total);
    if (!avcc) return 0;
    size_t off = 0;
    for (int i = 0; i < n; i++) {
        if (nals[i].n < 1) continue;
        uint8_t b = nals[i].p[0];
        int is_param;
        if (!d->is_hevc) {
            int t = b & 0x1F;
            is_param = (t == 7 || t == 8 || t == 9 || t == 6);
        } else {
            int t = (b >> 1) & 0x3F;
            is_param = (t == 32 || t == 33 || t == 34 || t == 35 || t == 39);
        }
        if (is_param) continue;
        uint32_t sz = (uint32_t)nals[i].n;
        avcc[off++] = (uint8_t)((sz >> 24) & 0xFF);
        avcc[off++] = (uint8_t)((sz >> 16) & 0xFF);
        avcc[off++] = (uint8_t)((sz >> 8) & 0xFF);
        avcc[off++] = (uint8_t)(sz & 0xFF);
        memcpy(avcc + off, nals[i].p, nals[i].n);
        off += nals[i].n;
    }

    CMBlockBufferRef bb = NULL;
    OSStatus st = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, avcc, total,
                                                     kCFAllocatorNull, NULL, 0, total, 0, &bb);
    if (st != noErr || !bb) {
        free(avcc);
        return 0;
    }
    CMSampleBufferRef sb = NULL;
    st = CMSampleBufferCreateReady(kCFAllocatorDefault, bb, d->fmt, 1, 0, NULL, 0, NULL, &sb);
    if (st != noErr || !sb) {
        CFRelease(bb);
        free(avcc);
        return 0;
    }

    d->frames_in++;
    d->have_out = 0;
    d->out_written = 0;
    d->out = out;
    d->out_cap = out_cap;

    VTDecodeInfoFlags flags = 0;
    st = VTDecompressionSessionDecodeFrame(d->sess, sb, 0, NULL, &flags);
    if (dec_debug() && st != noErr)
        fprintf(stderr, "[dec] DecodeFrame st=%d (frames_in=%llu)\n", (int)st,
                (unsigned long long)d->frames_in);

    CFRelease(sb);
    CFRelease(bb);
    free(avcc);
    d->out = NULL;
    d->out_cap = 0;

    if (!d->have_out) return 0;
    return d->out_written;
}

void yuduan_dec_destroy(void *h) {
    Dec *d = (Dec *)h;
    if (!d) return;
    teardown_session(d);
    free(d->sps);
    free(d->pps);
    free(d->vps);
    free(d);
}
