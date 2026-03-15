/* Test mouse drag by warping pointer and checking server logs.
 * Uses XWarpPointer to physically move the cursor. */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    Display *dpy = XOpenDisplay(":0");
    if (!dpy) { fprintf(stderr, "Cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);

    /* Find xterm's text window (deepest mapped child) */
    Window target = 0;
    {
        Window rr, pr, *ch; unsigned int n;
        XQueryTree(dpy, root, &rr, &pr, &ch, &n);
        for (unsigned i = 0; i < n; i++) {
            XWindowAttributes a;
            XGetWindowAttributes(dpy, ch[i], &a);
            if (a.map_state == IsViewable && a.width > 50) {
                Window rr2, pr2, *ch2; unsigned int n2;
                XQueryTree(dpy, ch[i], &rr2, &pr2, &ch2, &n2);
                for (unsigned j = 0; j < n2; j++) {
                    XWindowAttributes a2;
                    XGetWindowAttributes(dpy, ch2[j], &a2);
                    if (a2.map_state == IsViewable && a2.width > 50) {
                        target = ch2[j];
                        break;
                    }
                }
                if (ch2) XFree(ch2);
                if (target) break;
            }
        }
        if (ch) XFree(ch);
    }
    if (!target) { fprintf(stderr, "No target window\n"); return 1; }

    XWindowAttributes attr;
    XGetWindowAttributes(dpy, target, &attr);
    printf("Target: 0x%08lx (%dx%d) mask=0x%lx\n", target, attr.width, attr.height, attr.all_event_masks);

    /* Warp pointer to center of target */
    int cx = attr.width / 2;
    int cy = attr.height * 3 / 4; /* bottom quarter */
    printf("Warping to (%d, %d)\n", cx, cy);
    XWarpPointer(dpy, None, target, 0,0,0,0, cx, cy);
    XFlush(dpy);
    sleep(1);

    printf("Now drag selection upward (simulated by warping). Check server logs.\n");
    /* Warp upward step by step - this won't create a "drag" without a real button press,
       but it shows if MotionNotify is being sent */
    for (int y = cy; y >= -30; y -= 20) {
        XWarpPointer(dpy, None, target, 0,0,0,0, cx, y);
        XFlush(dpy);
        printf("  warped to (%d, %d)\n", cx, y);
        usleep(100000);
    }

    printf("Done. Check /tmp/pslx.log for MotionNotify and grab events.\n");
    XCloseDisplay(dpy);
    return 0;
}
