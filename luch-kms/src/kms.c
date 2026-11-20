// luch-kms: lowest-latency full-screen capture.
//
// Reads the scanout framebuffer straight from KMS the moment the compositor flips it (no compositor
// copy, no PipeWire), imports it into EGL/GLES (Mesa understands every Intel modifier, including the
// CCS-compressed ones the media engine can't read), scales + converts RGB->NV12 (BT.709, limited
// range) with two shader passes straight into a VA-API surface, and encodes it with h264_vaapi
// (async_depth=1, Sunshine/ALVR settings). Same design as Sunshine's kmsgrab + EGL path.
//
// Needs CAP_SYS_ADMIN for drmModeGetFB2 handles (as Sunshine's kmsgrab).
// stdout: packets, each [u32 len BE][u8 flags: bit0 IDR][u64 capture_us BE (CLOCK_MONOTONIC)][len bytes]
// stdin:  'k' = force IDR, EOF = quit.   --bench: no output, timing on stderr.
// The hardware cursor plane is read alongside and blended in the shader (as Sunshine does).
// SPDX-License-Identifier: GPL-3.0-or-later
#define EGL_EGLEXT_PROTOTYPES
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/hwcontext_vaapi.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static int64_t mono_us(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000000LL + t.tv_nsec / 1000; }
#define DIE(...) do { fprintf(stderr, "luch-kms: " __VA_ARGS__); fputc('\n', stderr); exit(1); } while (0)
static void ck(int r, const char *what) { if (r < 0) { char s[128]; av_strerror(r, s, sizeof s); DIE("%s: %s", what, s); } }

static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC img_to_tex;

// ---------- EGL / GLES ----------

static EGLDisplay egl_init(void) {
    EGLDisplay d = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL)) DIE("EGL init failed");
    eglBindAPI(EGL_OPENGL_ES_API);
    const EGLint ctx_attr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
    EGLContext ctx = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attr);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) DIE("EGL context failed");
    img_to_tex = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!img_to_tex) DIE("no glEGLImageTargetTexture2DOES");
    return d;
}

/// EGLImage over one layer of a DMA-BUF (up to 4 planes, one fd).
static EGLImage egl_import(EGLDisplay d, int fd, uint32_t fourcc, int w, int h, uint64_t mod, int nplanes,
                           const uint32_t *offsets, const uint32_t *pitches) {
    static const EGLint fd_k[] = { EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE3_FD_EXT };
    static const EGLint off_k[] = { EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT };
    static const EGLint pit_k[] = { EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT };
    static const EGLint lo_k[] = { EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT };
    static const EGLint hi_k[] = { EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT };
    EGLAttrib a[64]; int n = 0;
    a[n++] = EGL_WIDTH; a[n++] = w; a[n++] = EGL_HEIGHT; a[n++] = h; a[n++] = EGL_LINUX_DRM_FOURCC_EXT; a[n++] = fourcc;
    for (int i = 0; i < nplanes; i++) {
        a[n++] = fd_k[i]; a[n++] = fd; a[n++] = off_k[i]; a[n++] = offsets[i]; a[n++] = pit_k[i]; a[n++] = pitches[i];
        a[n++] = lo_k[i]; a[n++] = (EGLAttrib)(mod & 0xffffffff); a[n++] = hi_k[i]; a[n++] = (EGLAttrib)(mod >> 32);
    }
    a[n++] = EGL_NONE;
    EGLImage img = eglCreateImage(d, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
    if (img == EGL_NO_IMAGE) fprintf(stderr, "luch-kms: eglCreateImage(%.4s mod 0x%llx) failed: 0x%x\n", (char *)&fourcc, (unsigned long long)mod, eglGetError());
    return img;
}

static GLuint shader(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024]; glGetShaderInfoLog(s, sizeof log, NULL, log); DIE("shader: %s", log); }
    return s;
}

static GLuint program(const char *frag) {
    static const char *vs =
        "#version 300 es\n"
        "out vec2 uv;\n"
        "void main() { vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
        "  uv = p; gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0); }\n";
    GLuint p = glCreateProgram();
    glAttachShader(p, shader(GL_VERTEX_SHADER, vs));
    glAttachShader(p, shader(GL_FRAGMENT_SHADER, frag));
    glLinkProgram(p);
    GLint ok; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) DIE("link failed");
    return p;
}

// BT.709 limited range. The scanout texture is sampled as external (any modifier/format).
#define FRAG_HEAD "#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require\nprecision highp float;\n" \
                  "uniform samplerExternalOES src; uniform samplerExternalOES cur; uniform vec4 crect; uniform bool has_cur;\n" \
                  "in vec2 uv; out vec4 o;\n" \
                  "vec3 px() { vec3 c = texture(src, uv).rgb;\n" \
                  "  if (has_cur) { vec2 q = (uv - crect.xy) / crect.zw;\n" \
                  "    if (all(greaterThanEqual(q, vec2(0.0))) && all(lessThan(q, vec2(1.0)))) {\n" \
                  "      vec4 k = texture(cur, q); c = k.rgb + c * (1.0 - k.a); } }\n" \
                  "  return c; }\n"
static const char *FRAG_Y = FRAG_HEAD
    "void main() { vec3 c = px();\n"
    "  o = vec4(dot(c, vec3(0.2126, 0.7152, 0.0722)) * (219.0/255.0) + 16.0/255.0, 0.0, 0.0, 1.0); }\n";
static const char *FRAG_UV = FRAG_HEAD
    "void main() { vec3 c = px();\n"
    "  float u = dot(c, vec3(-0.1146, -0.3854, 0.5)) * (224.0/255.0) + 128.0/255.0;\n"
    "  float v = dot(c, vec3(0.5, -0.4542, -0.0458)) * (224.0/255.0) + 128.0/255.0;\n"
    "  o = vec4(u, v, 0.0, 1.0); }\n";

// ---------- caches ----------

/// One NV12 VA surface from the encoder pool, with its two planes bound as GL render targets.
typedef struct { VASurfaceID id; GLuint fbo[2]; } Target;
static Target targets[16]; static int n_targets;

/// Imported scanout buffer, keyed by KMS framebuffer id (the compositor cycles a few of them).
typedef struct { uint32_t fb_id; GLuint tex; EGLImage img; int fd; int w, h; } Source;
/// Separate caches: a new cursor buffer must never evict the primary buffer being sampled.
typedef struct { Source s[8]; int next; } Cache;
static Cache primary_cache, cursor_cache;

static Target *target_for(EGLDisplay d, AVFrame *f, int W, int H) {
    VASurfaceID id = (VASurfaceID)(uintptr_t)f->data[3];
    for (int i = 0; i < n_targets; i++) if (targets[i].id == id) return &targets[i];
    if (n_targets == 16) DIE("too many encoder surfaces");
    AVFrame *drm = av_frame_alloc();
    drm->format = AV_PIX_FMT_DRM_PRIME;
    ck(av_hwframe_map(drm, f, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_WRITE), "export VA surface");
    const AVDRMFrameDescriptor *dd = (void *)drm->data[0];
    Target *t = &targets[n_targets++];
    t->id = id;
    for (int l = 0; l < 2; l++) { // FFmpeg exports NV12 as two layers: R8 (Y) and GR88 (UV)
        const AVDRMLayerDescriptor *L = &dd->layers[dd->nb_layers == 2 ? l : 0];
        const AVDRMPlaneDescriptor *P = &L->planes[dd->nb_layers == 2 ? 0 : l];
        const AVDRMObjectDescriptor *O = &dd->objects[P->object_index];
        uint32_t off = P->offset, pitch = P->pitch;
        uint32_t fourcc = l == 0 ? DRM_FORMAT_R8 : DRM_FORMAT_GR88;
        EGLImage img = egl_import(d, O->fd, fourcc, l == 0 ? W : W / 2, l == 0 ? H : H / 2, O->format_modifier, 1, &off, &pitch);
        if (img == EGL_NO_IMAGE) DIE("cannot bind encoder surface plane %d", l);
        GLuint tex; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
        img_to_tex(GL_TEXTURE_2D, img);
        glGenFramebuffers(1, &t->fbo[l]); glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[l]);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) DIE("encoder plane %d not renderable", l);
    }
    av_frame_free(&drm); // EGL images keep their own reference to the buffer
    return t;
}

static Source *source_for(Cache *cache, EGLDisplay d, int card, uint32_t fb_id, int *w, int *h) {
    Source *sources = cache->s;
    for (int i = 0; i < 8; i++) if (sources[i].fb_id == fb_id && sources[i].tex) { *w = sources[i].w; *h = sources[i].h; return &sources[i]; }
    drmModeFB2 *fb = drmModeGetFB2(card, fb_id);
    if (!fb || !fb->handles[0]) { if (fb) drmModeFreeFB2(fb); return NULL; }
    int fd = -1;
    drmPrimeHandleToFD(card, fb->handles[0], DRM_CLOEXEC | DRM_RDWR, &fd);
    int np = 0; while (np < 4 && fb->handles[np]) np++;
    EGLImage img = egl_import(d, fd, fb->pixel_format, fb->width, fb->height, fb->modifier, np, fb->offsets, fb->pitches);
    *w = fb->width; *h = fb->height;
    for (int i = 0; i < 4; i++) if (fb->handles[i] && (i == 0 || fb->handles[i] != fb->handles[0])) {
        struct drm_gem_close gc = { .handle = fb->handles[i] }; drmIoctl(card, DRM_IOCTL_GEM_CLOSE, &gc);
    }
    drmModeFreeFB2(fb);
    if (img == EGL_NO_IMAGE) { close(fd); return NULL; }
    Source *s = &sources[cache->next++ % 8];
    if (s->tex) { glDeleteTextures(1, &s->tex); eglDestroyImage(d, s->img); close(s->fd); }
    s->fb_id = fb_id; s->img = img; s->fd = fd; s->w = *w; s->h = *h;
    glGenTextures(1, &s->tex); glBindTexture(GL_TEXTURE_EXTERNAL_OES, s->tex);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    img_to_tex(GL_TEXTURE_EXTERNAL_OES, img);
    return s;
}

/// Atomic property ids of a plane, looked up once by name.
typedef struct { uint32_t id, fb, x, y, w, h; } PlaneProps;

static uint64_t prop_by_name(int fd, uint32_t obj, const char *name, uint32_t *prop_id) {
    drmModeObjectProperties *pr = drmModeObjectGetProperties(fd, obj, DRM_MODE_OBJECT_PLANE);
    uint64_t v = 0;
    for (uint32_t i = 0; pr && i < pr->count_props; i++) {
        drmModePropertyRes *r = drmModeGetProperty(fd, pr->props[i]);
        if (r && !strcmp(r->name, name)) { v = pr->prop_values[i]; if (prop_id) *prop_id = r->prop_id; }
        drmModeFreeProperty(r);
    }
    drmModeFreeObjectProperties(pr);
    return v;
}

/// The cursor plane that can sit on the primary plane's CRTC (0 if none).
static uint32_t find_cursor(int fd, uint32_t primary, PlaneProps *pp) {
    drmModePlane *pl = drmModeGetPlane(fd, primary);
    drmModeRes *res = drmModeGetResources(fd);
    int idx = -1;
    for (int i = 0; res && i < res->count_crtcs; i++) if (res->crtcs[i] == pl->crtc_id) idx = i;
    drmModeFreeResources(res); drmModeFreePlane(pl);
    drmModePlaneRes *pr = drmModeGetPlaneResources(fd);
    uint32_t found = 0;
    for (uint32_t i = 0; pr && i < pr->count_planes && !found; i++) {
        drmModePlane *p = drmModeGetPlane(fd, pr->planes[i]);
        if (p && idx >= 0 && (p->possible_crtcs & (1u << idx)) && prop_by_name(fd, p->plane_id, "type", NULL) == DRM_PLANE_TYPE_CURSOR)
            found = p->plane_id;
        drmModeFreePlane(p);
    }
    drmModeFreePlaneResources(pr);
    if (found) {
        pp->id = found;
        prop_by_name(fd, found, "FB_ID", &pp->fb); prop_by_name(fd, found, "CRTC_X", &pp->x);
        prop_by_name(fd, found, "CRTC_Y", &pp->y); prop_by_name(fd, found, "CRTC_W", &pp->w);
        prop_by_name(fd, found, "CRTC_H", &pp->h);
    }
    return found;
}

/// Current cursor state: fb (0 = hidden) and its rectangle on the CRTC in pixels.
typedef struct { uint32_t fb; int64_t x, y, w, h; } Cursor;
static Cursor read_cursor(int fd, const PlaneProps *pp) {
    Cursor c = {0};
    drmModeObjectProperties *pr = drmModeObjectGetProperties(fd, pp->id, DRM_MODE_OBJECT_PLANE);
    for (uint32_t i = 0; pr && i < pr->count_props; i++) {
        uint32_t id = pr->props[i]; uint64_t v = pr->prop_values[i];
        if (id == pp->fb) c.fb = (uint32_t)v;
        else if (id == pp->x) c.x = (int64_t)v; // CRTC_X/Y are signed (cursor may hang off the edge)
        else if (id == pp->y) c.y = (int64_t)v;
        else if (id == pp->w) c.w = (int64_t)v;
        else if (id == pp->h) c.h = (int64_t)v;
    }
    drmModeFreeObjectProperties(pr);
    return c;
}

/// The biggest active plane: the desktop's primary plane on the main monitor.
static uint32_t find_plane(int fd) {
    drmModePlaneRes *pr = drmModeGetPlaneResources(fd);
    if (!pr) DIE("no plane resources");
    uint32_t best = 0, area = 0;
    for (uint32_t i = 0; i < pr->count_planes; i++) {
        drmModePlane *p = drmModeGetPlane(fd, pr->planes[i]);
        if (p && p->fb_id) {
            drmModeFB2 *fb = drmModeGetFB2(fd, p->fb_id);
            if (fb && fb->width * fb->height > area) { area = fb->width * fb->height; best = p->plane_id; }
            if (fb) drmModeFreeFB2(fb);
        }
        if (p) drmModeFreePlane(p);
    }
    drmModeFreePlaneResources(pr);
    return best;
}

int luch_kms_main(int argc, char **argv) {
    const char *card_path = getenv("LUCH_KMS_CARD") ? getenv("LUCH_KMS_CARD") : "/dev/dri/card1";
    const char *render = getenv("LUCH_KMS_RENDER") ? getenv("LUCH_KMS_RENDER") : "/dev/dri/renderD128";
    int W = 1920, H = 1080, FPS = 60, KBPS = 20000, bench = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--bench")) bench = 1;
        else if (!strcmp(argv[i], "--size") && i + 1 < argc) sscanf(argv[++i], "%dx%d", &W, &H);
        else if (!strcmp(argv[i], "--kbps") && i + 1 < argc) KBPS = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--fps") && i + 1 < argc) FPS = atoi(argv[++i]);
    }
    prctl(PR_SET_PDEATHSIG, SIGTERM); // die with luchd

    int card = open(card_path, O_RDWR | O_CLOEXEC);
    if (card < 0) DIE("open %s: %s", card_path, strerror(errno));
    drmSetClientCap(card, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);
    drmSetClientCap(card, DRM_CLIENT_CAP_ATOMIC, 1);
    uint32_t plane = find_plane(card);
    if (!plane) DIE("no active plane on %s (needs CAP_SYS_ADMIN)", card_path);

    PlaneProps cpp = {0};
    uint32_t cursor_plane = find_cursor(card, plane, &cpp);
    if (!cursor_plane) fprintf(stderr, "luch-kms: no cursor plane; the cursor will be missing unless the compositor draws it\n");

    EGLDisplay egl = egl_init();
    GLuint prog_y = program(FRAG_Y), prog_uv = program(FRAG_UV);
    GLuint progs[2] = { prog_y, prog_uv };
    for (int i = 0; i < 2; i++) {
        glUseProgram(progs[i]);
        glUniform1i(glGetUniformLocation(progs[i], "src"), 0);
        glUniform1i(glGetUniformLocation(progs[i], "cur"), 1);
    }

    // Encoder and its NV12 surface pool (the GL passes render straight into these surfaces).
    AVBufferRef *drm_dev = NULL, *va_dev = NULL;
    ck(av_hwdevice_ctx_create(&drm_dev, AV_HWDEVICE_TYPE_DRM, render, NULL, 0), "drm device");
    ck(av_hwdevice_ctx_create_derived(&va_dev, AV_HWDEVICE_TYPE_VAAPI, drm_dev, 0), "vaapi device");
    AVBufferRef *pool = av_hwframe_ctx_alloc(va_dev);
    AVHWFramesContext *pf = (void *)pool->data;
    pf->format = AV_PIX_FMT_VAAPI; pf->sw_format = AV_PIX_FMT_NV12; pf->width = W; pf->height = H; pf->initial_pool_size = 6;
    ck(av_hwframe_ctx_init(pool), "surface pool");
    const AVCodec *codec = avcodec_find_encoder_by_name("h264_vaapi");
    AVCodecContext *c = avcodec_alloc_context3(codec);
    c->width = W; c->height = H; c->time_base = (AVRational){1, 1000000}; c->framerate = (AVRational){FPS, 1};
    c->pix_fmt = AV_PIX_FMT_VAAPI; c->hw_frames_ctx = av_buffer_ref(pool);
    c->max_b_frames = 0; c->refs = 1; c->gop_size = INT16_MAX; c->keyint_min = INT16_MAX;
    c->flags |= AV_CODEC_FLAG_CLOSED_GOP | AV_CODEC_FLAG_LOW_DELAY; c->flags2 |= AV_CODEC_FLAG2_FAST;
    c->bit_rate = c->rc_max_rate = (int64_t)KBPS * 1000; c->rc_buffer_size = (int)((int64_t)KBPS * 1000 / FPS);
    c->color_range = AVCOL_RANGE_MPEG; c->colorspace = AVCOL_SPC_BT709; c->color_primaries = AVCOL_PRI_BT709; c->color_trc = AVCOL_TRC_BT709;
    c->compression_level = 7;
    AVDictionary *o = NULL;
    av_dict_set(&o, "async_depth", "1", 0); av_dict_set(&o, "rc_mode", "VBR", 0);
    av_dict_set(&o, "idr_interval", "2147483647", 0); av_dict_set(&o, "sei", "0", 0);
    ck(avcodec_open2(c, codec, &o), "open h264_vaapi");
    av_dict_free(&o);
    fprintf(stderr, "luch-kms: plane %u -> %dx%d h264 %d kbps (EGL convert, VA encode)\n", plane, W, H, KBPS);

    AVPacket *pkt = av_packet_alloc();
    uint32_t last_fb = 0;
    Cursor last_cur = {0};
    int force_idr = 0, frames = 0;
    double s_grab = 0, s_conv = 0, s_enc = 0;
    int64_t last_out = 0, stat_t = mono_us();
    const int64_t keepalive_us = 2000000 / FPS; // resend at fps/2 while the screen is static

    for (;;) {
        // Control: 'k' = IDR, EOF = quit. The 1 ms poll is also the flip-detection tick.
        struct pollfd pfd = { .fd = bench ? -1 : 0, .events = POLLIN };
        if (poll(&pfd, 1, 1) > 0) {
            char cmd[16];
            ssize_t n = read(0, cmd, sizeof cmd);
            if (n <= 0) break;
            if (memchr(cmd, 'k', n)) force_idr = 1;
        }
        int64_t t0 = mono_us();
        drmModePlane *p = drmModeGetPlane(card, plane);
        if (!p) continue;
        uint32_t fb_id = p->fb_id;
        drmModeFreePlane(p);
        // A moving hardware cursor doesn't flip the primary plane, so it counts as a change too.
        Cursor cur = cursor_plane ? read_cursor(card, &cpp) : (Cursor){0};
        int cur_moved = memcmp(&cur, &last_cur, sizeof cur) != 0;
        if (!fb_id || (fb_id == last_fb && !cur_moved && t0 - last_out < keepalive_us && !force_idr)) continue;
        last_fb = fb_id; last_cur = cur;
        int sw = 0, sh = 0;
        Source *s = source_for(&primary_cache, egl, card, fb_id, &sw, &sh);
        if (!s) continue;
        int cw = 0, ch = 0;
        Source *cs = cur.fb ? source_for(&cursor_cache, egl, card, cur.fb, &cw, &ch) : NULL;

        AVFrame *f = av_frame_alloc();
        ck(av_hwframe_get_buffer(pool, f, 0), "surface");
        Target *t = target_for(egl, f, W, H);
        int64_t t1 = mono_us();

        glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_EXTERNAL_OES, s->tex);
        if (cs) { glActiveTexture(GL_TEXTURE1); glBindTexture(GL_TEXTURE_EXTERNAL_OES, cs->tex); }
        // Cursor rectangle in source UV space; CRTC_W/H is the plane size (the cursor buffer is cropped to it).
        float rect[4] = { (float)cur.x / sw, (float)cur.y / sh, (float)(cur.w ? cur.w : cw) / sw, (float)(cur.h ? cur.h : ch) / sh };
        for (int i = 0; i < 2; i++) {
            glUseProgram(progs[i]);
            glUniform1i(glGetUniformLocation(progs[i], "has_cur"), cs != NULL);
            glUniform4fv(glGetUniformLocation(progs[i], "crect"), 1, rect);
        }
        glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[0]); glViewport(0, 0, W, H);
        glUseProgram(prog_y); glDrawArrays(GL_TRIANGLES, 0, 3);
        glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[1]); glViewport(0, 0, W / 2, H / 2);
        glUseProgram(prog_uv); glDrawArrays(GL_TRIANGLES, 0, 3);
        glFinish(); // ponytail: CPU wait for the GPU; an EGL native fence would let VA wait on the GPU instead
        int64_t t2 = mono_us();

        f->pts = t0;
        if (force_idr) { f->pict_type = AV_PICTURE_TYPE_I; f->flags |= AV_FRAME_FLAG_KEY; force_idr = 0; }
        ck(avcodec_send_frame(c, f), "send_frame");
        av_frame_free(&f);
        while (avcodec_receive_packet(c, pkt) == 0) {
            if (!bench) {
                uint8_t h[13]; uint32_t len = pkt->size; uint64_t cap = (uint64_t)t0;
                for (int i = 0; i < 4; i++) h[i] = len >> (24 - 8 * i);
                h[4] = (pkt->flags & AV_PKT_FLAG_KEY) ? 1 : 0;
                for (int i = 0; i < 8; i++) h[5 + i] = cap >> (56 - 8 * i);
                if (fwrite(h, 1, 13, stdout) != 13 || fwrite(pkt->data, 1, len, stdout) != len) return 0;
                fflush(stdout);
            }
            av_packet_unref(pkt);
        }
        int64_t t3 = mono_us();
        last_out = t3;
        frames++; s_grab += (t1 - t0) / 1e3; s_conv += (t2 - t1) / 1e3; s_enc += (t3 - t2) / 1e3;
        if (t3 - stat_t >= 1000000) {
            fprintf(stderr, "luch-kms: %d fps | grab %.2f ms, convert %.2f ms, encode %.2f ms, total %.2f ms (src %dx%d)\n",
                    frames, s_grab / frames, s_conv / frames, s_enc / frames, (s_grab + s_conv + s_enc) / frames, sw, sh);
            frames = 0; s_grab = s_conv = s_enc = 0; stat_t = t3;
        }
    }
    return 0;
}
