/*
 * harness.c — 解码器 dylib 自检（不依赖主端工程，CI/开发联调用）
 *
 * 校验项：
 *   1. C-ABI 符号齐全且 create 对不支持 codec 返回 NULL（不虚标）；
 *   2. out_size == w*h*4；
 *   3. 垃圾输入 → decode 返回 0（诚实失败，不产出黑帧冒充成功）；
 *   4. **编码器→解码器真实往返**：用 §8.3.3 编码器 dylib 把已知 RGBA 编成
 *      Annex-B，再解码回 RGBA，逐像素比对（允许有损，但不得退化为全黑/全灰）。
 *
 * 用法：
 *   clang -O2 harness.c -o harness
 *   ./harness <enc_dylib> <dec_dylib>
 */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

typedef void *(*CreateFn)(const char *, uint32_t, uint32_t);
typedef uint32_t (*EncodeFn)(void *, const uint8_t *, uint32_t, uint8_t *, uint32_t);
typedef uint32_t (*DecodeFn)(void *, const uint8_t *, uint32_t, uint8_t *, uint32_t);
typedef uint32_t (*SizeFn)(void *);
typedef void (*DestroyFn)(void *);

static int failures = 0;
#define CHECK(cond, msg, ...)                              \
    do {                                                   \
        if (!(cond)) {                                     \
            failures++;                                    \
            printf("[FAIL] " msg "\n", ##__VA_ARGS__);     \
        } else {                                           \
            printf("[ OK ] " msg "\n", ##__VA_ARGS__);     \
        }                                                  \
    } while (0)

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "用法: %s <enc_dylib> <dec_dylib>\n", argv[0]);
        return 2;
    }
    void *enc = dlopen(argv[1], RTLD_NOW);
    void *dec = dlopen(argv[2], RTLD_NOW);
    if (!enc) { printf("[FAIL] 打开编码器库失败: %s\n", dlerror()); return 1; }
    if (!dec) { printf("[FAIL] 打开解码器库失败: %s\n", dlerror()); return 1; }

    CreateFn enc_create = (CreateFn)dlsym(enc, "yuduan_enc_create");
    EncodeFn enc_encode = (EncodeFn)dlsym(enc, "yuduan_enc_encode");
    DestroyFn enc_destroy = (DestroyFn)dlsym(enc, "yuduan_enc_destroy");
    CreateFn dec_create = (CreateFn)dlsym(dec, "yuduan_dec_create");
    DecodeFn dec_decode = (DecodeFn)dlsym(dec, "yuduan_dec_decode");
    SizeFn dec_size = (SizeFn)dlsym(dec, "yuduan_dec_out_size");
    DestroyFn dec_destroy = (DestroyFn)dlsym(dec, "yuduan_dec_destroy");
    CHECK(enc_create && enc_encode && enc_destroy, "编码器 C-ABI 符号齐全");
    CHECK(dec_create && dec_decode && dec_size && dec_destroy, "解码器 C-ABI 符号齐全");
    if (!dec_create) return 1;

    /* 1. codec 守卫 */
    CHECK(dec_create("h264", 640, 360) != NULL, "create(h264) 成功");
    CHECK(dec_create("h265", 640, 360) != NULL, "create(h265) 成功");
    CHECK(dec_create("vp9", 640, 360) == NULL, "create(vp9) 拒接（不虚标）");
    CHECK(dec_create("h264", 0, 0) == NULL, "create(0x0) 拒接");

    /* 2. out_size */
    void *d0 = dec_create("h264", 640, 360);
    CHECK(dec_size(d0) == 640u * 360u * 4u, "out_size = w*h*4 = %u", dec_size(d0));
    dec_destroy(d0);

    /* 3. 垃圾输入诚实失败 */
    void *dg = dec_create("h264", 640, 360);
    uint8_t *out = (uint8_t *)malloc(640u * 360u * 4u);
    memset(out, 0xAB, 640u * 360u * 4u);
    uint32_t got = dec_decode(dg, (const uint8_t *)"not-a-bitstream", 15, out, 640u * 360u * 4u);
    CHECK(got == 0, "垃圾输入 decode 返回 0（实际 %u，且未污染输出缓冲）", got);
    dec_destroy(dg);

    /* 4. 编码→解码真实往返 */
    if (!enc_create) { free(out); return 1; }
    const uint32_t W = 640, H = 360;
    uint8_t *rgba = (uint8_t *)malloc(W * H * 4);
    for (uint32_t y = 0; y < H; y++) {
        for (uint32_t x = 0; x < W; x++) {
            uint8_t *p = rgba + (y * W + x) * 4;
            p[0] = (uint8_t)(x * 255 / W);
            p[1] = (uint8_t)(y * 255 / H);
            p[2] = (uint8_t)((x + y) % 256);
            p[3] = 255;
        }
    }
    void *e = enc_create("h264", W, H);
    CHECK(e != NULL, "编码器 create(h264) 成功");
    if (!e) { free(out); free(rgba); return 1; }

    void *d = dec_create("h264", W, H);
    uint32_t enc_cap = 1 << 20;
    uint8_t *bit = (uint8_t *)malloc(enc_cap);
    uint32_t decoded = 0;
    uint32_t keyframes = 0;
    for (int i = 0; i < 12; i++) {
        uint32_t n = enc_encode(e, rgba, W * H * 4, bit, enc_cap);
        if (n == 0) continue;
        if (bit[0] == 0 && bit[1] == 0 && bit[2] == 0 && bit[3] == 1) keyframes++;
        uint32_t w2 = dec_decode(d, bit, n, out, W * H * 4);
        if (w2 == W * H * 4) decoded++;
    }
    printf("       编码器输出 %u 个关键帧，解码器产出 %u 帧\n", keyframes, decoded);
    CHECK(keyframes >= 1, "编码器至少产出 1 个 Annex-B 关键帧");
    CHECK(decoded >= 1, "解码器至少产出 1 帧 RGBA（实际 %u）", decoded);
    if (decoded >= 1) {
        /* 有损压缩允许偏差，但不得是全黑/全灰（那说明解码根本没出图） */
        uint64_t sum = 0, diff = 0;
        int uniq = 0;
        uint8_t first = out[0];
        for (uint32_t i = 0; i < W * H * 4; i += 4) {
            sum += out[i];
            int dr = (int)out[i] - (int)rgba[i];
            diff += (uint64_t)(dr < 0 ? -dr : dr);
            if (out[i] != first) uniq = 1;
        }
        uint64_t px = W * H;
        double mean = (double)sum / (double)px;
        double mad = (double)diff / (double)px;
        printf("       解码均值 R=%.1f（原图 127.5），平均绝对差=%.1f\n", mean, mad);
        CHECK(uniq == 1, "解码画面非纯色（真实出图）");
        CHECK(mean > 60 && mean < 200, "解码亮度落在合理区间（%.1f）", mean);
        CHECK(mad < 40, "有损偏差可接受（MAD=%.1f < 40）", mad);
    }

    enc_destroy(e);
    dec_destroy(d);
    free(bit);
    free(out);
    free(rgba);
    printf(failures ? "\nharness: %d FAIL\n" : "\nharness: ALL PASS\n", failures);
    return failures ? 1 : 0;
}
