/* Test mouse drag selection via XSendEvent + XWarpPointer.
 * Sends ButtonPress, warps up, then ButtonRelease to simulate selection drag. */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <unistd.h>

Window find_deepest(Display *dpy, Window parent, int depth) {
    if (depth > 5) return 0;
    Window rr, pr, *ch; unsigned int n;
    if (!XQueryTree(dpy, parent, &rr, &pr, &ch, &n)) return 0;
    Window result = 0;
    for (unsigned i = 0; i < n && !result; i++) {
        XWindowAttributes a;
        if (XGetWindowAttributes(dpy, ch[i], &a) && a.map_state == IsViewable && a.width > 50) {
            Window deeper = find_deepest(dpy, ch[i], depth + 1);
            result = deeper ? deeper : ch[i];
        }
    }
    if (ch) XFree(ch);
    return result;
}

int main(void) {
    Display *dpy = XOpenDisplay(":0");
    if (!dpy) { fprintf(stderr, "Cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);
    Window target = find_deepest(dpy, root, 0);
    if (!target) { fprintf(stderr, "No target window\n"); return 1; }

    XWindowAttributes attr;
    XGetWindowAttributes(dpy, target, &attr);
    printf("Target: 0x%08lx (%dx%d) mask=0x%lx\n", target, attr.width, attr.height, attr.all_event_masks);

    int cx = attr.width / 2;
    int start_y = attr.height * 3 / 4;

    /* Warp to start position */
    printf("Warping to (%d, %d)\n", cx, start_y);
    XWarpPointer(dpy, None, target, 0,0,0,0, cx, start_y);
    XFlush(dpy);
    usleep(200000);

    /* Send ButtonPress */
    printf("Sending ButtonPress at (%d, %d)\n", cx, start_y);
    {
        XEvent ev = {0};
        ev.xbutton.type = ButtonPress;
        ev.xbutton.window = target;
        ev.xbutton.root = root;
        ev.xbutton.subwindow = None;
        ev.xbutton.x = cx;
        ev.xbutton.y = start_y;
        ev.xbutton.x_root = cx;
        ev.xbutton.y_root = start_y;
        ev.xbutton.button = Button1;
        ev.xbutton.state = 0;
        ev.xbutton.same_screen = True;
        ev.xbutton.time = 0;
        XSendEvent(dpy, target, True, ButtonPressMask, &ev);
        XFlush(dpy);
    }
    usleep(100000);

    /* Drag upward by warping + sending MotionNotify */
    printf("Dragging upward...\n");
    for (int y = start_y; y >= -30; y -= 15) {
        XWarpPointer(dpy, None, target, 0,0,0,0, cx, y);
        /* Also send explicit MotionNotify with Button1 state */
        XEvent ev = {0};
        ev.xmotion.type = MotionNotify;
        ev.xmotion.window = target;
        ev.xmotion.root = root;
        ev.xmotion.subwindow = None;
        ev.xmotion.x = cx;
        ev.xmotion.y = y;
        ev.xmotion.x_root = cx;
        ev.xmotion.y_root = y;
        ev.xmotion.state = Button1Mask;
        ev.xmotion.is_hint = NotifyNormal;
        ev.xmotion.same_screen = True;
        ev.xmotion.time = 0;
        XSendEvent(dpy, target, True, Button1MotionMask | ButtonMotionMask | PointerMotionMask, &ev);
        XFlush(dpy);
        printf("  dragged to (%d, %d)\n", cx, y);
        usleep(80000);
    }

    /* Send ButtonRelease */
    printf("Sending ButtonRelease\n");
    {
        XEvent ev = {0};
        ev.xbutton.type = ButtonRelease;
        ev.xbutton.window = target;
        ev.xbutton.root = root;
        ev.xbutton.subwindow = None;
        ev.xbutton.x = cx;
        ev.xbutton.y = -30;
        ev.xbutton.x_root = cx;
        ev.xbutton.y_root = -30;
        ev.xbutton.button = Button1;
        ev.xbutton.state = Button1Mask;
        ev.xbutton.same_screen = True;
        ev.xbutton.time = 0;
        XSendEvent(dpy, target, True, ButtonReleaseMask, &ev);
        XFlush(dpy);
    }

    printf("Done. Check /tmp/pslx.log\n");
    XCloseDisplay(dpy);
    return 0;
}
