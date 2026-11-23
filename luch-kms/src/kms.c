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
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static int64_t mono_us(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000000LL + t.tv_nsec / 1000; }
#define DIE(...) do { fprintf(stderr, "luch-kms: " __VA_ARGS__); fputc('\n', stderr); exit(1); } while (0)
static void ck(int r, const char *what) { if (r < 0) { char s[128]; av_strerror(r, s, sizeof s); DIE("%s: %s", what, s); } }


// Mesa's radeonsi HEVC encoder leaves slice_loop_filter_across_slices_enabled_flag out of the slice header, while the PPS
// that ffmpeg writes says pps_loop_filter_across_slices_enabled_flag = 1, i.e. that the flag is there: every decoder
// reads a broken header and shows garbage. When the first IDR slice shows the flag missing, the flag is cleared in each
// PPS we send (one slice per picture, so across-slice filtering is no loss). An encoder that writes it correctly is left alone.
static int hevc_pps_fix = -1; // -1 not decided yet, 0 stream is fine, 1 clear the flag in the PPS

static int rbit(const uint8_t *b, int pos) { return (b[pos >> 3] >> (7 - (pos & 7))) & 1; }
static unsigned rue(const uint8_t *b, int *pos) {
    int z = 0;
    while (z < 24 && !rbit(b, *pos)) { (*pos)++; z++; }
    (*pos)++;
    unsigned v = 0;
    for (int i = 0; i < z; i++) v = v << 1 | rbit(b, (*pos)++);
    return (1u << z) - 1 + v;
}

static void hevc_fix_pps(uint8_t *d, int n) {
    uint8_t *pps = NULL;
    for (int i = 0; i + 4 < n; i++) {
        if (d[i] || d[i + 1] || d[i + 2] != 1) continue;
        uint8_t *nal = d + i + 3;
        int t = (nal[0] >> 1) & 0x3f, left_in_nal = n - (i + 3);
        if (t == 34) pps = nal;
        if ((t != 19 && t != 20) || hevc_pps_fix >= 0) continue;
        uint8_t r[24] = {0};
        for (int k = 0, m = 0; k < 24 && k < left_in_nal; k++) {
            if (k >= 2 && nal[k] == 3 && !nal[k - 1] && !nal[k - 2]) continue; // emulation prevention
            r[m++] = nal[k];
        }
        int pos = 16; // after the NAL unit header
        if (!rbit(r, pos++)) continue; // first_slice_segment_in_pic_flag
        pos++; // no_output_of_prior_pics_flag
        rue(r, &pos); // slice_pic_parameter_set_id
        if (rue(r, &pos) != 2) continue; // an I slice
        pos += 2; // slice_sao_luma_flag, slice_sao_chroma_flag (the SPS has SAO on)
        rue(r, &pos); // slice_qp_delta
        int rem = 8 - (pos & 7), tail = r[pos >> 3] & ((1 << rem) - 1);
        int aligned_here = tail == (1 << (rem - 1)); // a 1 and zeros up to the byte end: byte_alignment() right now
        if (rem == 1 && aligned_here && r[(pos >> 3) + 1] == 0x80) aligned_here = 0; // a flag, the alignment in the next byte
        hevc_pps_fix = aligned_here;
        fprintf(stderr, "luch-kms: HEVC slice header %s slice_loop_filter_across_slices_enabled_flag%s\n",
                aligned_here ? "lacks" : "has", aligned_here ? ": clearing it in the PPS" : "");
    }
    if (hevc_pps_fix != 1 || !pps) return;
    // Find the flag in the PPS by its syntax (the first 6 bytes must not hold emulation prevention).
    uint8_t r[16] = {0};
    for (int k = 0; k < 16; k++) r[k] = pps[k];
    if (r[2] == 0 && r[3] == 0 && r[4] == 3) return;
    int pos = 16;
    rue(r, &pos); rue(r, &pos);            // pps_id, sps_id
    pos += 1 + 1 + 3 + 1 + 1;              // dependent_slice_segments, output_flag_present, extra bits, sign hiding, cabac_init
    rue(r, &pos); rue(r, &pos); rue(r, &pos); // num_ref_idx l0, l1, init_qp
    pos += 1 + 1;                          // constrained_intra_pred, transform_skip
    int cu_qp_delta = rbit(r, pos++);
    if (cu_qp_delta) rue(r, &pos);         // diff_cu_qp_delta_depth
    rue(r, &pos); rue(r, &pos);            // cb, cr qp offsets
    pos += 1 + 1 + 1 + 1;                  // slice chroma qp offsets present, weighted pred, bipred, transquant bypass
    int tiles = rbit(r, pos++);
    pos++;                                 // entropy_coding_sync
    if (tiles || (pos >> 3) >= 8) return;
    pps[pos >> 3] &= ~(0x80 >> (pos & 7)); // pps_loop_filter_across_slices_enabled_flag := 0
}

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
                  "uniform samplerExternalOES src; uniform samplerExternalOES cur; uniform vec4 crect; uniform bool has_cur; uniform int taps;\n" \
                  "in vec2 uv; out vec4 o;\n" \
                  "vec3 px_at(vec2 p) { vec3 c = texture(src, p).rgb;\n" \
                  "  if (has_cur) { vec2 q = (p - crect.xy) / crect.zw;\n" \
                  "    if (all(greaterThanEqual(q, vec2(0.0))) && all(lessThan(q, vec2(1.0)))) {\n" \
                  "      vec4 k = texture(cur, q); c = k.rgb + c * (1.0 - k.a); } }\n" \
                  "  return c; }\n" \
                  /* One bilinear tap per output pixel reads only 2x2 source texels: downscaling by more than ~1.3x then skips   */ \
                  /* texels unevenly and thin strokes (text) double or shimmer. Average taps x taps samples over the footprint. */ \
                  "vec3 px() { if (taps <= 1) return px_at(uv);\n" \
                  "  vec2 dx = dFdx(uv), dy = dFdy(uv); vec3 sum = vec3(0.0); float n = float(taps);\n" \
                  "  for (int i = 0; i < taps; i++) for (int j = 0; j < taps; j++)\n" \
                  "    sum += px_at(uv + ((float(i) + 0.5) / n - 0.5) * dx + ((float(j) + 0.5) / n - 0.5) * dy);\n" \
                  "  return sum / (n * n); }\n"
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
typedef struct { VASurfaceID id; GLuint fbo[2]; int geo[4]; } Target; // geo: picture rectangle drawn last (x, y, w, h)
static Target targets[16]; static int n_targets;

/// Imported scanout buffer, keyed by KMS framebuffer id (the compositor cycles a few of them).
typedef struct { uint32_t fb_id; ino_t ino; GLuint tex; EGLImage img; int fd; int w, h; } Source;
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
    // The coded picture is a multiple of 16 rows high (1088 for 1080): the extra rows are shown by a receiver that does not crop,
    // so they are bound and cleared to black too instead of staying green.
    int AH = (H + 15) & ~15;
    for (int l = 0; l < 2; l++) { // FFmpeg exports NV12 as two layers: R8 (Y) and GR88 (UV)
        const AVDRMLayerDescriptor *L = &dd->layers[dd->nb_layers == 2 ? l : 0];
        const AVDRMPlaneDescriptor *P = &L->planes[dd->nb_layers == 2 ? 0 : l];
        const AVDRMObjectDescriptor *O = &dd->objects[P->object_index];
        uint32_t off = P->offset, pitch = P->pitch;
        uint32_t fourcc = l == 0 ? DRM_FORMAT_R8 : DRM_FORMAT_GR88;
        EGLImage img = egl_import(d, O->fd, fourcc, l == 0 ? W : W / 2, l == 0 ? AH : AH / 2, O->format_modifier, 1, &off, &pitch);
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
    // KMS recycles framebuffer ids for new buffers, so the cache key is the buffer itself:
    // the DMA-BUF inode is stable per GEM object. A stale hit would replay an old frame.
    Source *sources = cache->s;
    drmModeFB2 *fb = drmModeGetFB2(card, fb_id);
    if (!fb || !fb->handles[0]) { if (fb) drmModeFreeFB2(fb); return NULL; }
    int fd = -1;
    drmPrimeHandleToFD(card, fb->handles[0], DRM_CLOEXEC | DRM_RDWR, &fd);
    for (int i = 0; i < 4; i++) if (fb->handles[i] && (i == 0 || fb->handles[i] != fb->handles[0])) {
        struct drm_gem_close gc = { .handle = fb->handles[i] }; drmIoctl(card, DRM_IOCTL_GEM_CLOSE, &gc);
    }
    struct stat st;
    ino_t ino = (fd >= 0 && fstat(fd, &st) == 0) ? st.st_ino : 0;
    for (int i = 0; i < 8; i++) if (sources[i].tex && sources[i].fb_id == fb_id && sources[i].ino == ino) {
        close(fd); drmModeFreeFB2(fb);
        *w = sources[i].w; *h = sources[i].h; return &sources[i];
    }
    int np = 0; while (np < 4 && fb->handles[np]) np++;
    EGLImage img = egl_import(d, fd, fb->pixel_format, fb->width, fb->height, fb->modifier, np, fb->offsets, fb->pitches);
    *w = fb->width; *h = fb->height;
    drmModeFreeFB2(fb);
    if (img == EGL_NO_IMAGE) { close(fd); return NULL; }
    Source *s = &sources[cache->next++ % 8];
    if (s->tex) { glDeleteTextures(1, &s->tex); eglDestroyImage(d, s->img); close(s->fd); }
    s->fb_id = fb_id; s->ino = ino; s->img = img; s->fd = fd; s->w = *w; s->h = *h;
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

/// The biggest active plane without an alpha channel (gamescope keeps the game there and draws the Steam overlay on a bigger
/// ARGB plane: taking that one shows a black picture); the biggest of any kind if every plane has alpha.
static uint32_t find_plane(int fd) {
    drmModePlaneRes *pr = drmModeGetPlaneResources(fd);
    if (!pr) DIE("no plane resources");
    uint32_t best = 0; uint64_t area = 0;
    for (uint32_t i = 0; i < pr->count_planes; i++) {
        drmModePlane *p = drmModeGetPlane(fd, pr->planes[i]);
        if (p && p->fb_id) {
            drmModeFB2 *fb = drmModeGetFB2(fd, p->fb_id);
            uint64_t score = fb ? (uint64_t)fb->width * fb->height + ((fb->pixel_format & 0xff) == 'A' ? 0 : 1ull << 40) : 0;
            if (fb && score > area) { area = score; best = p->plane_id; }
            if (fb) drmModeFreeFB2(fb);
        }
        if (p) drmModeFreePlane(p);
    }
    drmModeFreePlaneResources(pr);
    return best;
}

enum { SCALE_FIT, SCALE_FILL, SCALE_STRETCH };

int luch_kms_main(int argc, char **argv) {
    const char *card_path = getenv("LUCH_KMS_CARD");
    const char *render = getenv("LUCH_KMS_RENDER") ? getenv("LUCH_KMS_RENDER") : "/dev/dri/renderD128";
    int W = 1920, H = 1080, FPS = 60, KBPS = 20000, bench = 0;
    const char *codec_name = "h264_vaapi";
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--bench")) bench = 1;
        else if (!strcmp(argv[i], "--size") && i + 1 < argc) sscanf(argv[++i], "%dx%d", &W, &H);
        else if (!strcmp(argv[i], "--kbps") && i + 1 < argc) KBPS = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--fps") && i + 1 < argc) FPS = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--codec") && i + 1 < argc) { i++; codec_name = !strcmp(argv[i], "hevc") ? "hevc_vaapi" : "h264_vaapi"; }
    }
    prctl(PR_SET_PDEATHSIG, SIGTERM); // die with luchd
    const char *scale_env = getenv("LUCH_ASPECT");
    int scale_mode = !scale_env ? SCALE_FIT : !strcmp(scale_env, "fill") ? SCALE_FILL : !strcmp(scale_env, "stretch") ? SCALE_STRETCH : SCALE_FIT;

    // The card driving a display: LUCH_KMS_CARD, or the first /dev/dri/cardN with an active plane.
    int card = -1; uint32_t plane = 0; char probe[32];
    for (int n = 0; n < 8 && !plane; n++) {
        if (card_path && n) break;
        snprintf(probe, sizeof probe, "/dev/dri/card%d", n);
        const char *path = card_path ? card_path : probe;
        if (card >= 0) close(card);
        if ((card = open(path, O_RDWR | O_CLOEXEC)) < 0) continue;
        drmSetClientCap(card, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);
        drmSetClientCap(card, DRM_CLIENT_CAP_ATOMIC, 1);
        drmModePlaneRes *pr = drmModeGetPlaneResources(card);
        if (pr) { drmModeFreePlaneResources(pr); plane = find_plane(card); }
    }
    if (!plane) DIE("no active display plane on any /dev/dri/card* (luch-kms needs CAP_SYS_ADMIN)");

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
    const AVCodec *codec = avcodec_find_encoder_by_name(codec_name);
    if (!codec) DIE("%s not available", codec_name);
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
    ck(avcodec_open2(c, codec, &o), codec_name);
    av_dict_free(&o);
    fprintf(stderr, "luch-kms: plane %u -> %dx%d %s %d kbps, scale %s (EGL convert, VA encode)\n", plane, W, H, codec_name, KBPS,
            scale_mode == SCALE_FILL ? "fill" : scale_mode == SCALE_STRETCH ? "stretch" : "fit");

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
        // Scale the source into the encoded frame. LUCH_ASPECT: "fit" (default) keeps the aspect ratio and adds black bars
        // (a 21:9 screen on a 16:9 receiver); "fill" keeps it and crops the overflow; "stretch" fills the frame, distorted.
        // Even offsets and sizes keep the chroma plane aligned.
        float fit = scale_mode == SCALE_FILL ? fmaxf((float)W / sw, (float)H / sh) : fminf((float)W / sw, (float)H / sh);
        int dw = scale_mode == SCALE_STRETCH ? W : (int)(sw * fit + 0.5f) & ~1;
        int dh = scale_mode == SCALE_STRETCH ? H : (int)(sh * fit + 0.5f) & ~1;
        if (scale_mode == SCALE_FIT) { if (dw > W) dw = W; if (dh > H) dh = H; }
        int ox = ((W - dw) / 2) & ~1, oy = ((H - dh) / 2) & ~1;
        double ratio = fmax((double)sw / dw, (double)sh / dh);
        int taps = ratio <= 1.2 ? 1 : ratio <= 2.4 ? 2 : 3;
        for (int i = 0; i < 2; i++) { glUseProgram(progs[i]); glUniform1i(glGetUniformLocation(progs[i], "taps"), taps); }
        int geo[4] = { ox, oy, dw, dh };
        if (memcmp(t->geo, geo, sizeof geo) != 0) { // bars are black (limited range: Y 16, U and V 128); redraw them only on a change
            glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[0]); glClearColor(16.0f / 255, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
            glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[1]); glClearColor(128.0f / 255, 128.0f / 255, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
            memcpy(t->geo, geo, sizeof geo);
        }
        glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[0]); glViewport(ox, oy, dw, dh);
        glUseProgram(prog_y); glDrawArrays(GL_TRIANGLES, 0, 3);
        glBindFramebuffer(GL_FRAMEBUFFER, t->fbo[1]); glViewport(ox / 2, oy / 2, dw / 2, dh / 2);
        glUseProgram(prog_uv); glDrawArrays(GL_TRIANGLES, 0, 3);
        glFinish(); // ponytail: CPU wait for the GPU; an EGL native fence would let VA wait on the GPU instead
        int64_t t2 = mono_us();

        f->pts = t0;
        if (force_idr) { f->pict_type = AV_PICTURE_TYPE_I; f->flags |= AV_FRAME_FLAG_KEY; force_idr = 0; }
        ck(avcodec_send_frame(c, f), "send_frame");
        av_frame_free(&f);
        while (avcodec_receive_packet(c, pkt) == 0) {
            if ((pkt->flags & AV_PKT_FLAG_KEY) && !strcmp(codec_name, "hevc_vaapi") && av_packet_make_writable(pkt) >= 0)
                hevc_fix_pps(pkt->data, pkt->size);
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
