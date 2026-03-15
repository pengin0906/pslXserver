#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main() {
    Display *dpy = XOpenDisplay(":0");
    if (!dpy) { fprintf(stderr, "Cannot open display\n"); return 1; }

    // Find xterm window
    Window root = DefaultRootWindow(dpy);
    Window parent, *children;
    unsigned int nchildren;
    XQueryTree(dpy, root, &root, &parent, &children, &nchildren);

    if (nchildren == 0) {
        fprintf(stderr, "No windows found\n");
        return 1;
    }

    // Get first child (xterm top-level)
    Window target = children[0];
    printf("Target window: 0x%lx\n", target);

    // Get current geometry
    int x, y;
    unsigned int w, h, bw, depth;
    Window rret;
    XGetGeometry(dpy, target, &rret, &x, &y, &w, &h, &bw, &depth);
    printf("Current: %dx%d at (%d,%d)\n", w, h, x, y);

    // Test 1: Resize wider
    printf("\n=== Test 1: Resize to %dx%d ===\n", w + 100, h);
    XResizeWindow(dpy, target, w + 100, h);
    XSync(dpy, False);
    sleep(1);

    // Check new geometry
    XGetGeometry(dpy, target, &rret, &x, &y, &w, &h, &bw, &depth);
    printf("After widen: %dx%d\n", w, h);

    // Check child window too
    Window *ch2;
    unsigned int nch2;
    XQueryTree(dpy, target, &root, &parent, &ch2, &nch2);
    if (nch2 > 0) {
        int cx, cy;
        unsigned int cw, ch_h, cbw, cd;
        XGetGeometry(dpy, ch2[0], &rret, &cx, &cy, &cw, &ch_h, &cbw, &cd);
        printf("Child 0x%lx: %dx%d\n", ch2[0], cw, ch_h);
        XFree(ch2);
    }

    // Test 2: Resize taller
    printf("\n=== Test 2: Resize to %dx%d ===\n", w, h + 100);
    XResizeWindow(dpy, target, w, h + 100);
    XSync(dpy, False);
    sleep(1);

    XGetGeometry(dpy, target, &rret, &x, &y, &w, &h, &bw, &depth);
    printf("After tallen: %dx%d\n", w, h);

    // Check child
    XQueryTree(dpy, target, &root, &parent, &ch2, &nch2);
    if (nch2 > 0) {
        int cx, cy;
        unsigned int cw, ch_h, cbw, cd;
        XGetGeometry(dpy, ch2[0], &rret, &cx, &cy, &cw, &ch_h, &cbw, &cd);
        printf("Child 0x%lx: %dx%d\n", ch2[0], cw, ch_h);
        XFree(ch2);
    }

    // Test 3: Resize both
    printf("\n=== Test 3: Resize to %dx%d ===\n", w + 50, h + 50);
    XResizeWindow(dpy, target, w + 50, h + 50);
    XSync(dpy, False);
    sleep(1);

    XGetGeometry(dpy, target, &rret, &x, &y, &w, &h, &bw, &depth);
    printf("After both: %dx%d\n", w, h);

    XQueryTree(dpy, target, &root, &parent, &ch2, &nch2);
    if (nch2 > 0) {
        int cx, cy;
        unsigned int cw, ch_h, cbw, cd;
        XGetGeometry(dpy, ch2[0], &rret, &cx, &cy, &cw, &ch_h, &cbw, &cd);
        printf("Child 0x%lx: %dx%d\n", ch2[0], cw, ch_h);
        XFree(ch2);
    }

    // Test 4: Shrink back
    printf("\n=== Test 4: Shrink to 400x300 ===\n");
    XResizeWindow(dpy, target, 400, 300);
    XSync(dpy, False);
    sleep(1);

    XGetGeometry(dpy, target, &rret, &x, &y, &w, &h, &bw, &depth);
    printf("After shrink: %dx%d\n", w, h);

    XQueryTree(dpy, target, &root, &parent, &ch2, &nch2);
    if (nch2 > 0) {
        int cx, cy;
        unsigned int cw, ch_h, cbw, cd;
        XGetGeometry(dpy, ch2[0], &rret, &cx, &cy, &cw, &ch_h, &cbw, &cd);
        printf("Child 0x%lx: %dx%d\n", ch2[0], cw, ch_h);
        XFree(ch2);
    }

    if (children) XFree(children);
    XCloseDisplay(dpy);
    printf("\nDone.\n");
    return 0;
}
