// Low-latency H.264 encoder: DMA-BUF (NV12, any modifier) -> VA-API surface (zero copy) -> h264_vaapi.
// Settings follow Sunshine: async_depth=1, no B-frames, one reference, infinite GOP + IDR on demand,
// VBR with a one-frame VBV. SPDX-License-Identifier: GPL-3.0-or-later
#include <drm_fourcc.h>
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/opt.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef struct LuchEnc {
    AVBufferRef *drm_dev, *va_dev, *drm_frames, *va_frames;
    AVCodecContext *c;
    AVPacket *pkt;
    uint8_t *out;
    size_t out_cap;
    int64_t pts;
} LuchEnc;

void luch_enc_close(LuchEnc *e) {
    if (!e) return;
    avcodec_free_context(&e->c);
    av_packet_free(&e->pkt);
    av_buffer_unref(&e->drm_frames);
    av_buffer_unref(&e->va_frames);
    av_buffer_unref(&e->va_dev);
    av_buffer_unref(&e->drm_dev);
    av_free(e->out);
    av_free(e);
}

static int fail(char *err, size_t n, const char *what, int r) {
    char s[128];
    av_strerror(r, s, sizeof s);
    snprintf(err, n, "%s: %s", what, s);
    return r;
}

LuchEnc *luch_enc_open(const char *render_node, int w, int h, int fps, int kbps, char *err, size_t errlen) {
    LuchEnc *e = av_mallocz(sizeof *e);
    int r;
    if ((r = av_hwdevice_ctx_create(&e->drm_dev, AV_HWDEVICE_TYPE_DRM, render_node, NULL, 0)) < 0) { fail(err, errlen, "drm device", r); goto bad; }
    if ((r = av_hwdevice_ctx_create_derived(&e->va_dev, AV_HWDEVICE_TYPE_VAAPI, e->drm_dev, 0)) < 0) { fail(err, errlen, "vaapi device", r); goto bad; }

    e->drm_frames = av_hwframe_ctx_alloc(e->drm_dev);
    AVHWFramesContext *df = (AVHWFramesContext *)e->drm_frames->data;
    df->format = AV_PIX_FMT_DRM_PRIME; df->sw_format = AV_PIX_FMT_NV12; df->width = w; df->height = h;
    if ((r = av_hwframe_ctx_init(e->drm_frames)) < 0) { fail(err, errlen, "drm frames", r); goto bad; }

    e->va_frames = av_hwframe_ctx_alloc(e->va_dev);
    AVHWFramesContext *vf = (AVHWFramesContext *)e->va_frames->data;
    vf->format = AV_PIX_FMT_VAAPI; vf->sw_format = AV_PIX_FMT_NV12; vf->width = w; vf->height = h; vf->initial_pool_size = 0;
    if ((r = av_hwframe_ctx_init(e->va_frames)) < 0) { fail(err, errlen, "vaapi frames", r); goto bad; }

    const AVCodec *codec = avcodec_find_encoder_by_name("h264_vaapi");
    if (!codec) { snprintf(err, errlen, "h264_vaapi not available"); goto bad; }
    AVCodecContext *c = e->c = avcodec_alloc_context3(codec);
    c->width = w; c->height = h;
    c->time_base = (AVRational){1, 1000000}; // pts in microseconds
    c->framerate = (AVRational){fps, 1};
    c->pix_fmt = AV_PIX_FMT_VAAPI;
    c->hw_frames_ctx = av_buffer_ref(e->va_frames);
    c->max_b_frames = 0; c->refs = 1;
    c->gop_size = INT16_MAX; c->keyint_min = INT16_MAX;
    c->flags |= AV_CODEC_FLAG_CLOSED_GOP | AV_CODEC_FLAG_LOW_DELAY;
    c->flags2 |= AV_CODEC_FLAG2_FAST;
    c->bit_rate = c->rc_max_rate = (int64_t)kbps * 1000;
    c->rc_buffer_size = (int)((int64_t)kbps * 1000 / fps); // one-frame VBV: no frame overshoots the link
    c->color_range = AVCOL_RANGE_MPEG;
    AVDictionary *o = NULL;
    av_dict_set(&o, "async_depth", "1", 0);
    av_dict_set(&o, "rc_mode", "VBR", 0);
    av_dict_set(&o, "idr_interval", "2147483647", 0);
    av_dict_set(&o, "sei", "0", 0);
    r = avcodec_open2(c, codec, &o);
    av_dict_free(&o);
    if (r < 0) { fail(err, errlen, "open h264_vaapi", r); goto bad; }
    e->pkt = av_packet_alloc();
    return e;
bad:
    luch_enc_close(e);
    return NULL;
}

static void free_desc(void *opaque, uint8_t *data) { (void)opaque; av_free(data); }

/// Encodes one NV12 DMA-BUF frame. On success returns the packet size and points *out at it
/// (valid until the next call); 0 means the encoder produced nothing; <0 is an error.
int luch_enc_encode(LuchEnc *e, int fd, uint64_t modifier, uint32_t off0, uint32_t pitch0,
                    uint32_t off1, uint32_t pitch1, int64_t pts_us, int force_idr,
                    const uint8_t **out, int *is_key, char *err, size_t errlen) {
    int r;
    off_t size = lseek(fd, 0, SEEK_END);
    AVDRMFrameDescriptor *d = av_mallocz(sizeof *d);
    d->nb_objects = 1;
    d->objects[0].fd = fd;
    d->objects[0].size = size > 0 ? (size_t)size : 0;
    d->objects[0].format_modifier = modifier;
    d->nb_layers = 1;
    d->layers[0].format = DRM_FORMAT_NV12;
    d->layers[0].nb_planes = 2;
    d->layers[0].planes[0] = (AVDRMPlaneDescriptor){0, off0, pitch0};
    d->layers[0].planes[1] = (AVDRMPlaneDescriptor){0, off1, pitch1};

    AVFrame *src = av_frame_alloc(), *dst = av_frame_alloc();
    src->format = AV_PIX_FMT_DRM_PRIME;
    src->width = e->c->width; src->height = e->c->height;
    src->data[0] = (uint8_t *)d;
    src->buf[0] = av_buffer_create((uint8_t *)d, sizeof *d, free_desc, NULL, 0);
    src->hw_frames_ctx = av_buffer_ref(e->drm_frames);

    dst->format = AV_PIX_FMT_VAAPI;
    dst->hw_frames_ctx = av_buffer_ref(e->va_frames);
    if ((r = av_hwframe_map(dst, src, AV_HWFRAME_MAP_READ)) < 0) { fail(err, errlen, "map dmabuf->vaapi", r); goto done; }
    dst->pts = pts_us;
    if (force_idr) {
        dst->pict_type = AV_PICTURE_TYPE_I;
        dst->flags |= AV_FRAME_FLAG_KEY;
    }
    if ((r = avcodec_send_frame(e->c, dst)) < 0) { fail(err, errlen, "send_frame", r); goto done; }
    r = 0;
    while (avcodec_receive_packet(e->c, e->pkt) == 0) {
        if ((size_t)(r + e->pkt->size) > e->out_cap) {
            e->out_cap = (size_t)(r + e->pkt->size) * 2;
            e->out = av_realloc(e->out, e->out_cap);
        }
        memcpy(e->out + r, e->pkt->data, e->pkt->size);
        r += e->pkt->size;
        *is_key = !!(e->pkt->flags & AV_PKT_FLAG_KEY);
        av_packet_unref(e->pkt);
    }
    *out = e->out;
done:
    av_frame_free(&dst); // unmaps the VA surface; the DMA-BUF stays owned by the caller
    av_frame_free(&src);
    return r;
}
