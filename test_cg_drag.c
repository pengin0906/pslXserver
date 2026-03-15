/* Test drag selection using CGEvent to inject real mouse events.
 * Uses X11 to find the window position, then CGEvent for actual click+drag. */
#include <CoreGraphics/CoreGraphics.h>
#include <CoreFoundation/CoreFoundation.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    /* First, find xterm window geometry via X11 */
    Display *dpy = XOpenDisplay(":0");
    if (!dpy) { fprintf(stderr, "Cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);

    /* Find first visible top-level window */
    Window target_top = 0;
    int x11_w = 0, x11_h = 0;
    {
        Window rr, pr, *ch; unsigned int n;
        XQueryTree(dpy, root, &rr, &pr, &ch, &n);
        for (unsigned i = 0; i < n; i++) {
            XWindowAttributes a;
            if (XGetWindowAttributes(dpy, ch[i], &a) && a.map_state == IsViewable && a.width > 50) {
                target_top = ch[i];
                x11_w = a.width;
                x11_h = a.height;
                break;
            }
        }
        if (ch) XFree(ch);
    }
    XCloseDisplay(dpy);
    if (!target_top) { fprintf(stderr, "No window found\n"); return 1; }
    printf("X11 window 0x%lx size %dx%d\n", target_top, x11_w, x11_h);

    /* The X11 server positions window at macOS frame origin.
     * We need to find the screen position. Use CGWindowListCopyWindowInfo. */
    CFArrayRef windowList = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);

    CGRect winBounds = CGRectZero;
    int found = 0;
    if (windowList) {
        CFIndex count = CFArrayGetCount(windowList);
        for (CFIndex i = 0; i < count; i++) {
            CFDictionaryRef winInfo = CFArrayGetValueAtIndex(windowList, i);
            /* Match by size - look for window with matching width */
            CFDictionaryRef bounds;
            if (CFDictionaryGetValueIfPresent(winInfo, kCGWindowBounds, (const void **)&bounds)) {
                CGRectMakeWithDictionaryRepresentation(bounds, &winBounds);
                if ((int)winBounds.size.width == x11_w &&
                    (int)winBounds.size.height >= x11_h && (int)winBounds.size.height <= x11_h + 30) {
                    /* Check owner name contains pslX */
                    CFStringRef ownerName;
                    if (CFDictionaryGetValueIfPresent(winInfo, kCGWindowOwnerName, (const void **)&ownerName)) {
                        char buf[256];
                        CFStringGetCString(ownerName, buf, sizeof(buf), kCFStringEncodingUTF8);
                        printf("Found window: owner='%s' bounds=(%.0f,%.0f %.0fx%.0f)\n",
                            buf, winBounds.origin.x, winBounds.origin.y,
                            winBounds.size.width, winBounds.size.height);
                        found = 1;
                        break;
                    }
                }
            }
        }
        CFRelease(windowList);
    }

    if (!found) {
        fprintf(stderr, "Could not find matching macOS window\n");
        return 1;
    }

    /* Calculate click point: center-x, 3/4 down in content area */
    /* winBounds is in CGWindow coords (origin=top-left of screen, includes title bar) */
    double title_bar_h = winBounds.size.height - x11_h;
    double cx = winBounds.origin.x + x11_w / 2.0;
    double start_y = winBounds.origin.y + title_bar_h + x11_h * 3.0 / 4.0;

    printf("Click at screen (%.0f, %.0f), drag 300px up\n", cx, start_y);
    printf("Title bar height: %.0f\n", title_bar_h);

    CGPoint start = {cx, start_y};

    /* Mouse down */
    CGEventRef mouseDown = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseDown, start, kCGMouseButtonLeft);
    CGEventPost(kCGHIDEventTap, mouseDown);
    CFRelease(mouseDown);
    usleep(200000);

    /* Drag upward past window top */
    int steps = 40;
    double total_drag = start_y - winBounds.origin.y + 50;  /* past top of window */
    double dy = total_drag / steps;
    for (int i = 1; i <= steps; i++) {
        CGPoint p = {cx, start_y - i * dy};
        CGEventRef drag = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseDragged, p, kCGMouseButtonLeft);
        CGEventPost(kCGHIDEventTap, drag);
        CFRelease(drag);
        printf("  drag to (%.0f, %.0f)\n", p.x, p.y);
        usleep(60000);
    }

    /* Mouse up */
    CGPoint endPt = {cx, start_y - total_drag};
    CGEventRef mouseUp = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseUp, endPt, kCGMouseButtonLeft);
    CGEventPost(kCGHIDEventTap, mouseUp);
    CFRelease(mouseUp);
    printf("Released at (%.0f, %.0f)\n", endPt.x, endPt.y);
    printf("Done. Check /tmp/pslx.log\n");
    return 0;
}
