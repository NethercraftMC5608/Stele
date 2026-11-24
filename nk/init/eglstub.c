/* The EGL of last resort: a display that exists and can do nothing.
 *
 * WebKitWebProcess aborts during startup unless eglGetDisplay(EGL_DEFAULT_
 * DISPLAY) and eglInitialize both succeed -- PlatformDisplayDefault::create
 * calls CRASH() when they do not, and it runs even with compositing
 * disabled and Skia painting on the CPU, because the display is created
 * before any of that is decided. The integration image carries glvnd but
 * no vendor library, so the call fails and every render dies at "Could not
 * create default EGL display: EGL_BAD_PARAMETER. Aborting...".
 *
 * A real Mesa vendor is not the answer here. llvmpipe brings libLLVM, and
 * in the WebProcess its worker threads livelock nk's scheduler -- a
 * billion futex wakeups and no forward progress -- which is a worse
 * failure than the abort. So this stub exists instead: it answers the two
 * questions PlatformDisplayDefault asks and refuses everything else the
 * way EGL itself would -- no configs, no contexts, no extensions. Every
 * GLContext::create* path then fails politely, WebKit logs "Could not
 * create EGL context" and falls back to the software rendering the render
 * lane is configured to use anyway (WEBKIT_DISABLE_COMPOSITING_MODE,
 * WEBKIT_SKIA_ENABLE_CPU_RENDERING).
 *
 * Only the WebProcess may load it: render-next.py bind-mounts a wrapper
 * over WebKitWebProcess that puts this directory first in that one
 * process's LD_LIBRARY_PATH. The UI process keeps the honest glvnd,
 * because its Cairo fallback already works without EGL, and a stub that
 * answered its platform-display queries would break the host the way an
 * earlier blanket dummy EGL did.
 *
 * The headers are not included and the ABI is written out by hand: the
 * build container then needs nothing but gcc, with no apt and no network,
 * and the EGL entry points and their types are an ABI that does not move.
 */

typedef int EGLBoolean;
typedef int EGLint;
typedef unsigned int EGLenum;
typedef void *EGLDisplay;
typedef void *EGLConfig;
typedef void *EGLContext;
typedef void *EGLSurface;
typedef void *EGLImage;
typedef void *EGLSync;
typedef void *EGLClientBuffer;
typedef void *EGLNativeDisplayType;
typedef void *EGLNativePixmapType;
typedef void *EGLNativeWindowType;
typedef void *(*__eglMustCastToProperFunctionPointerType)(void);
/* EGLAttrib is pointer-sized; this file is only ever built for arm64. */
typedef long EGLAttrib;

#define EGL_FALSE 0
#define EGL_TRUE 1
#define EGL_SUCCESS 0x3000
#define EGL_NOT_INITIALIZED 0x3001
#define EGL_BAD_ACCESS 0x3002
#define EGL_BAD_ALLOC 0x3003
#define EGL_BAD_ATTRIBUTE 0x3004
#define EGL_BAD_CONFIG 0x3005
#define EGL_BAD_CONTEXT 0x3006
#define EGL_BAD_CURRENT_SURFACE 0x3007
#define EGL_BAD_DISPLAY 0x3008
#define EGL_BAD_MATCH 0x3009
#define EGL_BAD_SURFACE 0x300A
#define EGL_BAD_NATIVE_PIXMAP 0x300B
#define EGL_BAD_PARAMETER 0x300C
#define EGL_BAD_NATIVE_WINDOW 0x300D
#define EGL_CONTEXT_LOST 0x300E
#define EGL_DEFAULT_DISPLAY ((EGLNativeDisplayType)0)
#define EGL_NO_DISPLAY ((EGLDisplay)0)
#define EGL_NO_CONTEXT ((EGLContext)0)
#define EGL_NO_SURFACE ((EGLSurface)0)
#define EGL_NO_IMAGE ((EGLImage)0)
#define EGL_NO_SYNC ((EGLSync)0)
#define EGL_VENDOR 0x3053
#define EGL_VERSION 0x3054
#define EGL_EXTENSIONS 0x3055
#define EGL_CLIENT_APIS 0x308D
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_OPENVG_API 0x30A1
#define EGL_OPENGL_API 0x30A2

/* Non-zero, and not a plausible pointer: nobody should dereference it, and
 * EGL_NO_DISPLAY (NULL) must stay distinguishable. */
#define STUB_DISPLAY ((EGLDisplay)0x10)

/* Not thread-local on purpose. The EGL paths run once, on the WebProcess's
 * main thread during startup, and a dlopened library with TLS of its own
 * is exactly the machinery this stub exists to avoid exercising. */
static EGLint stub_error = EGL_SUCCESS;

static EGLBoolean stub_check_display(EGLDisplay display)
{
    if (display != STUB_DISPLAY) {
        stub_error = EGL_BAD_DISPLAY;
        return EGL_FALSE;
    }
    stub_error = EGL_SUCCESS;
    return EGL_TRUE;
}

/* The refusal every object-creating call shares: the display is real, and
 * the thing asked of it simply cannot be backed. */
static EGLBoolean stub_refuse(EGLDisplay display, EGLint error)
{
    if (!stub_check_display(display))
        return EGL_FALSE;
    stub_error = error;
    return EGL_FALSE;
}

EGLDisplay eglGetDisplay(EGLNativeDisplayType native_display)
{
    /* EGL_DEFAULT_DISPLAY and nothing else. Answering a Wayland or GBM
     * native pointer here would convince a caller that a platform display
     * exists -- which is precisely the lie that hung the UI process when
     * an earlier dummy EGL answered everything. */
    if (native_display == EGL_DEFAULT_DISPLAY) {
        stub_error = EGL_SUCCESS;
        return STUB_DISPLAY;
    }
    stub_error = EGL_BAD_PARAMETER;
    return EGL_NO_DISPLAY;
}

EGLBoolean eglInitialize(EGLDisplay display, EGLint *major, EGLint *minor)
{
    if (!stub_check_display(display))
        return EGL_FALSE;
    /* 1.4, not 1.5: below 1.5, WebKit reaches images through
     * eglGetProcAddress, which this stub answers with NULL, rather than
     * assuming the 1.5 entry points exist. */
    if (major)
        *major = 1;
    if (minor)
        *minor = 4;
    return EGL_TRUE;
}

EGLBoolean eglTerminate(EGLDisplay display)
{
    return stub_check_display(display);
}

const char *eglQueryString(EGLDisplay display, EGLint name)
{
    if (!stub_check_display(display))
        return 0;
    switch (name) {
    case EGL_VENDOR:
        return "nk stub";
    case EGL_VERSION:
        return "1.4 nk stub";
    case EGL_EXTENSIONS:
        /* Empty is load-bearing: every optional path in WebKit gates on an
         * extension, and an empty list turns them all off at once. */
        return "";
    case EGL_CLIENT_APIS:
        return "OpenGL_ES";
    default:
        stub_error = EGL_BAD_PARAMETER;
        return 0;
    }
}

/* A display with zero configs, reported the way a real one reports it:
 * success with nothing in the list, so callers walk their own fallbacks
 * rather than reading this as a broken display. */
EGLBoolean eglGetConfigs(EGLDisplay display, EGLConfig *configs,
                         EGLint config_size, EGLint *num_config)
{
    (void)configs;
    (void)config_size;
    if (!stub_check_display(display))
        return EGL_FALSE;
    if (!num_config) {
        stub_error = EGL_BAD_PARAMETER;
        return EGL_FALSE;
    }
    *num_config = 0;
    return EGL_TRUE;
}

EGLBoolean eglChooseConfig(EGLDisplay display, const EGLint *attrib_list,
                           EGLConfig *configs, EGLint config_size,
                           EGLint *num_config)
{
    (void)attrib_list;
    return eglGetConfigs(display, configs, config_size, num_config);
}

EGLBoolean eglGetConfigAttrib(EGLDisplay display, EGLConfig config,
                              EGLint attribute, EGLint *value)
{
    (void)config;
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_CONFIG);
}

EGLBoolean eglBindAPI(EGLenum api)
{
    /* Any API binds; the display still has nothing to make a context with,
     * and callers should reach that discovery rather than stop here. */
    (void)api;
    stub_error = EGL_SUCCESS;
    return EGL_TRUE;
}

EGLenum eglQueryAPI(void)
{
    return EGL_OPENGL_ES_API;
}

EGLContext eglCreateContext(EGLDisplay display, EGLConfig config,
                            EGLContext share_context, const EGLint *attrib_list)
{
    (void)config;
    (void)share_context;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_CONTEXT;
    stub_error = EGL_BAD_CONFIG;
    return EGL_NO_CONTEXT;
}

EGLBoolean eglDestroyContext(EGLDisplay display, EGLContext context)
{
    (void)context;
    return stub_check_display(display);
}

EGLBoolean eglMakeCurrent(EGLDisplay display, EGLSurface draw,
                          EGLSurface read, EGLContext context)
{
    (void)draw;
    (void)read;
    (void)context;
    return stub_refuse(display, EGL_BAD_MATCH);
}

EGLSurface eglCreateWindowSurface(EGLDisplay display, EGLConfig config,
                                  EGLNativeWindowType window,
                                  const EGLint *attrib_list)
{
    (void)config;
    (void)window;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSurface eglCreatePbufferSurface(EGLDisplay display, EGLConfig config,
                                   const EGLint *attrib_list)
{
    (void)config;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSurface eglCreatePixmapSurface(EGLDisplay display, EGLConfig config,
                                  EGLNativePixmapType pixmap,
                                  const EGLint *attrib_list)
{
    (void)config;
    (void)pixmap;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSurface eglCreatePbufferFromClientBuffer(EGLDisplay display, EGLenum buftype,
                                            EGLClientBuffer buffer,
                                            EGLConfig config,
                                            const EGLint *attrib_list)
{
    (void)buftype;
    (void)buffer;
    (void)config;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_CONFIG;
    return EGL_NO_SURFACE;
}

EGLBoolean eglDestroySurface(EGLDisplay display, EGLSurface surface)
{
    (void)surface;
    return stub_check_display(display);
}

EGLBoolean eglQuerySurface(EGLDisplay display, EGLSurface surface,
                           EGLint attribute, EGLint *value)
{
    (void)surface;
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_SURFACE);
}

EGLBoolean eglSurfaceAttrib(EGLDisplay display, EGLSurface surface,
                            EGLint attribute, EGLint value)
{
    (void)surface;
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_SURFACE);
}

EGLBoolean eglSwapBuffers(EGLDisplay display, EGLSurface surface)
{
    (void)surface;
    return stub_refuse(display, EGL_BAD_SURFACE);
}

EGLBoolean eglCopyBuffers(EGLDisplay display, EGLSurface surface,
                          EGLNativePixmapType target)
{
    (void)surface;
    (void)target;
    return stub_refuse(display, EGL_BAD_SURFACE);
}

EGLBoolean eglSwapInterval(EGLDisplay display, EGLint interval)
{
    (void)interval;
    return stub_refuse(display, EGL_BAD_SURFACE);
}

EGLBoolean eglWaitGL(void)
{
    return EGL_TRUE;
}

EGLBoolean eglWaitNative(EGLint engine)
{
    (void)engine;
    return EGL_TRUE;
}

EGLBoolean eglWaitClient(void)
{
    return EGL_TRUE;
}

EGLBoolean eglReleaseThread(void)
{
    return EGL_TRUE;
}

EGLDisplay eglGetCurrentDisplay(void)
{
    return EGL_NO_DISPLAY;
}

EGLContext eglGetCurrentContext(void)
{
    return EGL_NO_CONTEXT;
}

EGLSurface eglGetCurrentSurface(EGLint readdraw)
{
    (void)readdraw;
    return EGL_NO_SURFACE;
}

EGLBoolean eglQueryContext(EGLDisplay display, EGLContext context,
                           EGLint attribute, EGLint *value)
{
    (void)context;
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_CONTEXT);
}

__eglMustCastToProperFunctionPointerType eglGetProcAddress(const char *procname)
{
    /* NULL is EGL's "not available", and with no extensions advertised
     * nothing that asks should be calling what it gets back anyway. */
    (void)procname;
    stub_error = EGL_SUCCESS;
    return (__eglMustCastToProperFunctionPointerType)0;
}

EGLint eglGetError(void)
{
    EGLint error = stub_error;
    /* EGL clears the error once read; a caller that wants it twice should
     * have asked once. */
    stub_error = EGL_SUCCESS;
    return error;
}

/* EGL 1.5 core and the EXT spellings epoxy resolves by name. All of them
 * refuse: nothing on this display can back any of them. */
EGLDisplay eglGetPlatformDisplay(EGLenum platform, void *native_display,
                                 const EGLAttrib *attrib_list)
{
    (void)platform;
    (void)native_display;
    (void)attrib_list;
    stub_error = EGL_BAD_PARAMETER;
    return EGL_NO_DISPLAY;
}

EGLDisplay eglGetPlatformDisplayEXT(EGLenum platform, void *native_display,
                                    const EGLint *attrib_list)
{
    (void)platform;
    (void)native_display;
    (void)attrib_list;
    stub_error = EGL_BAD_PARAMETER;
    return EGL_NO_DISPLAY;
}

EGLSurface eglCreatePlatformWindowSurface(EGLDisplay display, EGLConfig config,
                                          void *native_window,
                                          const EGLAttrib *attrib_list)
{
    (void)config;
    (void)native_window;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSurface eglCreatePlatformWindowSurfaceEXT(EGLDisplay display, EGLConfig config,
                                             void *native_window,
                                             const EGLint *attrib_list)
{
    (void)config;
    (void)native_window;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSurface eglCreatePlatformPixmapSurface(EGLDisplay display, EGLConfig config,
                                          void *native_pixmap,
                                          const EGLAttrib *attrib_list)
{
    (void)config;
    (void)native_pixmap;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SURFACE;
    stub_error = EGL_BAD_MATCH;
    return EGL_NO_SURFACE;
}

EGLSync eglCreateSync(EGLDisplay display, EGLenum type, const EGLAttrib *attrib_list)
{
    (void)type;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_SYNC;
    stub_error = EGL_BAD_PARAMETER;
    return EGL_NO_SYNC;
}

EGLBoolean eglDestroySync(EGLDisplay display, EGLSync sync)
{
    (void)sync;
    return stub_refuse(display, EGL_BAD_PARAMETER);
}

EGLint eglClientWaitSync(EGLDisplay display, EGLSync sync, EGLint flags,
                         EGLAttrib timeout)
{
    (void)sync;
    (void)flags;
    (void)timeout;
    if (!stub_check_display(display))
        return EGL_FALSE;
    stub_error = EGL_BAD_PARAMETER;
    return EGL_FALSE;
}

EGLBoolean eglWaitSync(EGLDisplay display, EGLSync sync, EGLint flags)
{
    (void)sync;
    (void)flags;
    return stub_refuse(display, EGL_BAD_PARAMETER);
}

EGLBoolean eglGetSyncAttrib(EGLDisplay display, EGLSync sync, EGLint attribute,
                            EGLAttrib *value)
{
    (void)sync;
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_PARAMETER);
}

EGLImage eglCreateImage(EGLDisplay display, EGLContext context, EGLenum target,
                        EGLClientBuffer buffer, const EGLAttrib *attrib_list)
{
    (void)context;
    (void)target;
    (void)buffer;
    (void)attrib_list;
    if (!stub_check_display(display))
        return EGL_NO_IMAGE;
    stub_error = EGL_BAD_PARAMETER;
    return EGL_NO_IMAGE;
}

EGLBoolean eglDestroyImage(EGLDisplay display, EGLImage image)
{
    (void)image;
    return stub_refuse(display, EGL_BAD_PARAMETER);
}

EGLBoolean eglQueryDisplayAttribEXT(EGLDisplay display, EGLint attribute,
                                    EGLAttrib *value)
{
    (void)attribute;
    (void)value;
    return stub_refuse(display, EGL_BAD_PARAMETER);
}
