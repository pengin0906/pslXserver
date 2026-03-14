// Test macOS-side window resize (simulates drag resize)
// Uses AppKit to find and resize the Xserver window
#include <ApplicationServices/ApplicationServices.h>
#include <stdio.h>
#include <unistd.h>

int main() {
    // Get list of windows
    CFArrayRef windowList = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly, kCGNullWindowID);

    if (!windowList) {
        fprintf(stderr, "Cannot get window list\n");
        return 1;
    }

    int count = CFArrayGetCount(windowList);
    printf("Found %d windows\n", count);

    CGWindowID targetWinID = 0;
    for (int i = 0; i < count; i++) {
        CFDictionaryRef info = CFArrayGetValueAtIndex(windowList, i);
        CFStringRef ownerName;
        if (CFDictionaryGetValueIfPresent(info, kCGWindowOwnerName, (const void**)&ownerName)) {
            char buf[256];
            if (CFStringGetCString(ownerName, buf, sizeof(buf), kCFStringEncodingUTF8)) {
                if (strstr(buf, "Xserver") || strstr(buf, "xterm")) {
                    CFNumberRef winID;
                    if (CFDictionaryGetValueIfPresent(info, kCGWindowNumber, (const void**)&winID)) {
                        CGWindowID wid;
                        CFNumberGetValue(winID, kCFNumberIntType, &wid);

                        CFDictionaryRef bounds;
                        if (CFDictionaryGetValueIfPresent(info, kCGWindowBounds, (const void**)&bounds)) {
                            CGRect r;
                            CGRectMakeWithDictionaryRepresentation(bounds, &r);
                            printf("  Window %d (%s): %.0fx%.0f at (%.0f,%.0f)\n",
                                   wid, buf, r.size.width, r.size.height, r.origin.x, r.origin.y);
                            if (targetWinID == 0) targetWinID = wid;
                        }
                    }
                }
            }
        }
    }

    CFRelease(windowList);

    if (targetWinID == 0) {
        fprintf(stderr, "No Xserver window found\n");
        return 1;
    }

    printf("\nCannot directly resize another app's window via CG API.\n");
    printf("Use AppleScript instead.\n");
    return 0;
}
