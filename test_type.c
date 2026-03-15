/* Send keystrokes to xterm via XSendEvent */
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

Window find_xterm_window(Display *dpy, Window parent, int depth) {
    Window root_ret, parent_ret, *children;
    unsigned int n;
    if (depth > 5) return 0;
    if (!XQueryTree(dpy, parent, &root_ret, &parent_ret, &children, &n))
        return 0;
    Window result = 0;
    for (unsigned int i = 0; i < n && !result; i++) {
        XWindowAttributes a;
        if (XGetWindowAttributes(dpy, children[i], &a) && a.map_state == IsViewable) {
            if (a.width > 50 && a.height > 50) {
                Window deeper = find_xterm_window(dpy, children[i], depth + 1);
                result = deeper ? deeper : children[i];
            }
        }
    }
    if (children) XFree(children);
    return result;
}

void send_key(Display *dpy, Window win, KeySym ks, int shift) {
    KeyCode kc = XKeysymToKeycode(dpy, ks);
    XEvent ev = {0};
    ev.xkey.type = KeyPress;
    ev.xkey.window = win;
    ev.xkey.root = DefaultRootWindow(dpy);
    ev.xkey.state = shift ? ShiftMask : 0;
    ev.xkey.keycode = kc;
    ev.xkey.same_screen = True;
    XSendEvent(dpy, win, True, KeyPressMask, &ev);
    XFlush(dpy);
    usleep(10000);
    ev.xkey.type = KeyRelease;
    XSendEvent(dpy, win, True, KeyReleaseMask, &ev);
    XFlush(dpy);
    usleep(10000);
}

int main(int argc, char **argv) {
    Display *dpy = XOpenDisplay(":0");
    if (!dpy) { fprintf(stderr, "Cannot open display\n"); return 1; }
    Window win = find_xterm_window(dpy, DefaultRootWindow(dpy), 0);
    if (!win) { fprintf(stderr, "No window found\n"); return 1; }
    printf("Target: 0x%lx\n", win);

    const char *text = argc > 1 ? argv[1] : "ls -lR /\n";
    for (const char *p = text; *p; p++) {
        int shift = 0;
        KeySym ks;
        switch (*p) {
            case '\n': ks = XK_Return; break;
            case ' ':  ks = XK_space; break;
            case '-':  ks = XK_minus; break;
            case '/':  ks = XK_slash; break;
            case '.':  ks = XK_period; break;
            default:
                if (*p >= 'a' && *p <= 'z') ks = XK_a + (*p - 'a');
                else if (*p >= 'A' && *p <= 'Z') { ks = XK_a + (*p - 'A'); shift = 1; }
                else if (*p >= '0' && *p <= '9') ks = XK_0 + (*p - '0');
                else ks = *p;
                break;
        }
        send_key(dpy, win, ks, shift);
    }
    XCloseDisplay(dpy);
    printf("Done.\n");
    return 0;
}
