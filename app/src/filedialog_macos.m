#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <string.h>

// The caller owns the returned path. Zero means the user cancelled.
int swarm_macos_choose_path(const char *title, const char *start, bool folder, char **path) {
    __block int result = -1;
    void (^choose)(void) = ^{
        @autoreleasepool {
            if (NSApp == nil) {
                return;
            }

            NSOpenPanel *panel = [NSOpenPanel openPanel];
            panel.title = [NSString stringWithUTF8String:title];
            panel.canChooseFiles = !folder;
            panel.canChooseDirectories = folder;
            panel.canCreateDirectories = folder;
            panel.allowsMultipleSelection = NO;

            if (*start != '\0') {
                NSURL *directory = [NSURL fileURLWithFileSystemRepresentation:start
                                                                  isDirectory:YES
                                                                relativeToURL:nil];
                NSNumber *isDirectory = nil;
                if ([directory getResourceValue:&isDirectory forKey:NSURLIsDirectoryKey error:nil] &&
                    isDirectory.boolValue) {
                    panel.directoryURL = directory;
                }
            }

            if ([panel runModal] != NSModalResponseOK) {
                result = 0;
                return;
            }
            const char *chosen = panel.URL.fileSystemRepresentation;
            if (chosen != NULL && (*path = strdup(chosen)) != NULL) {
                result = 1;
            }
        }
    };
    if ([NSThread isMainThread]) {
        choose();
    } else {
        dispatch_sync(dispatch_get_main_queue(), choose);
    }
    return result;
}
