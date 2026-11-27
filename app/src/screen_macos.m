#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <math.h>

static const NSTimeInterval capture_timeout = 5;
static const NSTimeInterval picker_timeout = 120;

static NSString *capture_app_name(void) {
    NSURL *url = NSBundle.mainBundle.bundleURL;
    if ([url.pathExtension caseInsensitiveCompare:@"app"] == NSOrderedSame) {
        return url.lastPathComponent.stringByDeletingPathExtension;
    }
    return NSRunningApplication.currentApplication.localizedName ?: NSProcessInfo.processInfo.processName;
}

static NSString *capture_error(NSString *context, NSError *error) {
    if (!error) return context;
    return [NSString stringWithFormat:@"%@: %@ (%@ %ld)", context,
            error.localizedDescription, error.domain, (long)error.code];
}

static void copy_error(char *out, size_t size, NSString *message) {
    snprintf(out, size, "%s", message.UTF8String ?: "macOS screen capture failed");
}

// A picker selection authorizes its filter for this capture session,
// without requiring a persistent Screen Recording grant. All picker UI
// runs on the main queue; the capture thread waits for the observer.
@interface SwarmScreenPicker : NSObject <SCContentSharingPickerObserver> {
    NSCondition *_condition;
    SCContentFilter *_filter;
    NSString *_error;
    bool _done;
    bool _closed;
    bool _registered; // Main queue only.
}
- (void)present;
- (SCContentFilter *)waitForFilter:(NSString **)error;
- (void)close;
@end

static __weak SwarmScreenPicker *active_picker;

@implementation SwarmScreenPicker
- (instancetype)init {
    self = [super init];
    if (self) _condition = [[NSCondition alloc] init];
    return self;
}

- (void)present {
    [_condition lock];
    bool closed = _closed;
    [_condition unlock];
    if (closed) return;

    SCContentSharingPicker *picker = SCContentSharingPicker.sharedPicker;
    SCContentSharingPickerConfiguration *configuration = [[SCContentSharingPickerConfiguration alloc] init];
    configuration.allowedPickerModes = SCContentSharingPickerModeSingleDisplay;
    configuration.allowsChangingSelectedContent = NO;
    picker.defaultConfiguration = configuration;
    picker.maximumStreamCount = @1;
    [picker addObserver:self];
    _registered = true;
    active_picker = self;
    picker.active = YES;
    [picker presentPickerUsingContentStyle:SCShareableContentStyleDisplay];
}

- (void)completeWithFilter:(SCContentFilter *)filter error:(NSString *)error {
    [_condition lock];
    if (!_done && !_closed) {
        _filter = filter;
        _error = error;
        _done = true;
        [_condition signal];
    }
    [_condition unlock];
}

- (void)contentSharingPicker:(SCContentSharingPicker *)picker didCancelForStream:(SCStream *)stream {
    (void)picker;
    (void)stream;
    [self completeWithFilter:nil error:[NSString stringWithFormat:@"Screen selection for %@ was cancelled", capture_app_name()]];
}

- (void)contentSharingPicker:(SCContentSharingPicker *)picker didUpdateWithFilter:(SCContentFilter *)filter forStream:(SCStream *)stream {
    (void)picker;
    if (!stream) [self completeWithFilter:filter error:nil];
}

- (void)contentSharingPickerStartDidFailWithError:(NSError *)error {
    [self completeWithFilter:nil error:capture_error([NSString stringWithFormat:@"macOS could not show the screen selector for %@", capture_app_name()], error)];
}

- (SCContentFilter *)waitForFilter:(NSString **)error {
    [_condition lock];
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:picker_timeout];
    while (!_done && [_condition waitUntilDate:deadline]) {}
    if (!_done) {
        _done = true;
        _error = [NSString stringWithFormat:@"Screen selection for %@ timed out; try sharing again", capture_app_name()];
    }
    SCContentFilter *filter = _filter;
    *error = _error;
    [_condition unlock];
    return filter;
}

- (void)close {
    [_condition lock];
    _closed = true;
    _done = true;
    _filter = nil;
    [_condition signal];
    [_condition unlock];
    dispatch_async(dispatch_get_main_queue(), ^{
        SCContentSharingPicker *picker = SCContentSharingPicker.sharedPicker;
        if (self->_registered) {
            [picker removeObserver:self];
            self->_registered = false;
        }
        if (active_picker == self) {
            picker.active = NO;
            active_picker = nil;
        }
    });
}
@end

// All access to the pending frame and stream status is under this lock.
// Keep one borrowed IOSurface at most, so old frames can be recycled by
// ScreenCaptureKit even while the Rust thread is converting a frame.
@interface SwarmScreenOutput : NSObject <SCStreamOutput, SCStreamDelegate> {
    NSCondition *_condition;
    CVPixelBufferRef _latest;
    NSString *_ended;
}
- (CVPixelBufferRef)takeFrameWaiting:(bool)wait ended:(NSString **)ended CF_RETURNS_RETAINED;
- (void)finish:(NSString *)reason;
@end

@implementation SwarmScreenOutput
- (instancetype)init {
    self = [super init];
    if (self) {
        _condition = [[NSCondition alloc] init];
    }
    return self;
}

- (void)dealloc {
    if (_latest) CVPixelBufferRelease(_latest);
}

- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sample ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen || !CMSampleBufferIsValid(sample)) return;
    NSArray<NSDictionary *> *attachments = (__bridge NSArray *)CMSampleBufferGetSampleAttachmentsArray(sample, false);
    NSNumber *status = attachments.firstObject[SCStreamFrameInfoStatus];
    if (status && status.integerValue == SCFrameStatusStopped) {
        [self finish:@"macOS stopped sharing the screen"];
        return;
    }
    if (!status || (status.integerValue != SCFrameStatusComplete && status.integerValue != SCFrameStatusStarted)) return;
    CVPixelBufferRef frame = CMSampleBufferGetImageBuffer(sample);
    if (!frame) return;

    [_condition lock];
    if (!_ended) {
        if (_latest) CVPixelBufferRelease(_latest);
        _latest = CVPixelBufferRetain(frame);
        [_condition signal];
    }
    [_condition unlock];
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    [self finish:capture_error(@"macOS stopped sharing the screen", error)];
}

- (void)finish:(NSString *)reason {
    [_condition lock];
    _ended = reason;
    if (_latest) {
        CVPixelBufferRelease(_latest);
        _latest = NULL;
    }
    [_condition signal];
    [_condition unlock];
}

- (CVPixelBufferRef)takeFrameWaiting:(bool)wait ended:(NSString **)ended {
    [_condition lock];
    if (wait) {
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:capture_timeout];
        while (!_latest && !_ended && [_condition waitUntilDate:deadline]) {}
    }
    CVPixelBufferRef frame = _latest;
    _latest = NULL;
    *ended = _ended;
    [_condition unlock];
    return frame;
}
@end

@interface SwarmScreenCapture : NSObject
@property(nonatomic, strong) SCStream *stream;
@property(nonatomic, strong) SwarmScreenOutput *output;
@property(nonatomic, strong) SwarmScreenPicker *picker;
@property(nonatomic) size_t width;
@property(nonatomic) size_t height;
@end

@implementation SwarmScreenCapture
@end

static dispatch_time_t capture_deadline(void) {
    return dispatch_time(DISPATCH_TIME_NOW, (int64_t)(capture_timeout * NSEC_PER_SEC));
}

static void stop_stream(SCStream *stream, SwarmScreenOutput *output) {
    // Keep the objects alive until the asynchronous stop completes.
    [stream stopCaptureWithCompletionHandler:^(NSError *error) {
        (void)error;
        [stream removeStreamOutput:output type:SCStreamOutputTypeScreen error:NULL];
    }];
}

void *swarm_macos_screen_open(uint32_t *width, uint32_t *height, uint32_t fps, char *error, size_t error_size) {
    @autoreleasepool {
        SCContentFilter *filter = nil;
        SwarmScreenPicker *picker = nil;
        size_t pixels_wide = 0;
        size_t pixels_high = 0;
        if (CGPreflightScreenCaptureAccess()) {
            dispatch_semaphore_t found = dispatch_semaphore_create(0);
            __block SCShareableContent *content;
            __block NSError *content_error;
            [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *result, NSError *why) {
                content = result;
                content_error = why;
                dispatch_semaphore_signal(found);
            }];
            if (dispatch_semaphore_wait(found, capture_deadline())) {
                copy_error(error, error_size, @"macOS did not list the shareable displays in time");
                return NULL;
            }
            if (!content) {
                copy_error(error, error_size, capture_error([NSString stringWithFormat:@"macOS could not list screens for %@", capture_app_name()], content_error));
                return NULL;
            }

            SCDisplay *display = nil;
            CGDirectDisplayID main_display = CGMainDisplayID();
            for (SCDisplay *candidate in content.displays) {
                if (candidate.displayID == main_display) {
                    display = candidate;
                    break;
                }
            }
            pixels_wide = CGDisplayPixelsWide(main_display);
            pixels_high = CGDisplayPixelsHigh(main_display);
            if (!display || !pixels_wide || !pixels_high) {
                copy_error(error, error_size, @"macOS did not find the main display to share");
                return NULL;
            }
            filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        } else {
            // Command-line capture has no AppKit event loop to present a
            // picker. Keep the explicit grant/restart route for it.
            if (!NSApp || !NSApp.isRunning) {
                (void)CGRequestScreenCaptureAccess();
                copy_error(error, error_size, [NSString stringWithFormat:@"Allow %@ to record the screen in System Settings → Privacy & Security, then reopen %@; or share from its app window to choose a screen.", capture_app_name(), capture_app_name()]);
                return NULL;
            }
            picker = [[SwarmScreenPicker alloc] init];
            dispatch_async(dispatch_get_main_queue(), ^{ [picker present]; });
            NSString *selection_error = nil;
            filter = [picker waitForFilter:&selection_error];
            if (!filter) {
                [picker close];
                copy_error(error, error_size, selection_error);
                return NULL;
            }
            double selected_width = filter.contentRect.size.width * filter.pointPixelScale;
            double selected_height = filter.contentRect.size.height * filter.pointPixelScale;
            if (!isfinite(selected_width) || !isfinite(selected_height) || selected_width < 1 || selected_height < 1 || selected_width > UINT32_MAX || selected_height > UINT32_MAX) {
                [picker close];
                copy_error(error, error_size, @"macOS did not provide the selected screen's dimensions");
                return NULL;
            }
            pixels_wide = (size_t)ceil(selected_width);
            pixels_high = (size_t)ceil(selected_height);
        }

        double scale = fmin(1.0, fmin((double)*width / pixels_wide, (double)*height / pixels_high));
        SCStreamConfiguration *configuration = [[SCStreamConfiguration alloc] init];
        configuration.width = (size_t)(scale * pixels_wide);
        configuration.height = (size_t)(scale * pixels_high);
        configuration.pixelFormat = kCVPixelFormatType_32BGRA;
        configuration.minimumFrameInterval = CMTimeMake(1, fps);
        configuration.showsCursor = YES;

        SwarmScreenOutput *output = [[SwarmScreenOutput alloc] init];
        SCStream *stream = [[SCStream alloc] initWithFilter:filter configuration:configuration delegate:output];
        dispatch_queue_t samples = dispatch_queue_create("swarm.screen.frames", DISPATCH_QUEUE_SERIAL);
        NSError *output_error = nil;
        if (![stream addStreamOutput:output type:SCStreamOutputTypeScreen sampleHandlerQueue:samples error:&output_error]) {
            [picker close];
            copy_error(error, error_size, capture_error(@"macOS could not attach screen capture output", output_error));
            return NULL;
        }

        dispatch_semaphore_t ready = dispatch_semaphore_create(0);
        __block NSError *start_error;
        [stream startCaptureWithCompletionHandler:^(NSError *why) {
            start_error = why;
            dispatch_semaphore_signal(ready);
        }];
        if (dispatch_semaphore_wait(ready, capture_deadline())) {
            stop_stream(stream, output);
            [picker close];
            copy_error(error, error_size, @"macOS did not start screen sharing in time");
            return NULL;
        }
        if (start_error) {
            [stream removeStreamOutput:output type:SCStreamOutputTypeScreen error:NULL];
            [picker close];
            copy_error(error, error_size, capture_error([NSString stringWithFormat:@"macOS could not start screen sharing for %@", capture_app_name()], start_error));
            return NULL;
        }

        SwarmScreenCapture *capture = [[SwarmScreenCapture alloc] init];
        capture.stream = stream;
        capture.output = output;
        capture.picker = picker;
        capture.width = *width = (uint32_t)configuration.width;
        capture.height = *height = (uint32_t)configuration.height;
        return (__bridge_retained void *)capture;
    }
}

// 1: copied a frame; 0: nothing new; -1: the stream ended; -2: bad frame.
int swarm_macos_screen_read(void *handle, uint8_t *rgb, size_t rgb_size, bool wait, char *error, size_t error_size) {
    @autoreleasepool {
        SwarmScreenCapture *capture = (__bridge SwarmScreenCapture *)handle;
        NSString *ended = nil;
        CVPixelBufferRef frame = [capture.output takeFrameWaiting:wait ended:&ended];
        if (ended) {
            copy_error(error, error_size, ended);
            return -1;
        }
        if (!frame) return 0;

        size_t width = capture.width;
        size_t height = capture.height;
        if (rgb_size != width * height * 3 || CVPixelBufferGetWidth(frame) != width || CVPixelBufferGetHeight(frame) != height ||
            CVPixelBufferGetPixelFormatType(frame) != kCVPixelFormatType_32BGRA || CVPixelBufferLockBaseAddress(frame, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
            CVPixelBufferRelease(frame);
            copy_error(error, error_size, @"macOS did not provide a screen picture in the requested format");
            return -2;
        }
        const uint8_t *bgra = CVPixelBufferGetBaseAddress(frame);
        size_t stride = CVPixelBufferGetBytesPerRow(frame);
        bool readable = bgra && stride >= width * 4;
        if (readable) {
            for (size_t y = 0; y < height; y++) {
                const uint8_t *row = bgra + y * stride;
                for (size_t x = 0; x < width; x++) {
                    *rgb++ = row[x * 4 + 2];
                    *rgb++ = row[x * 4 + 1];
                    *rgb++ = row[x * 4];
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(frame, kCVPixelBufferLock_ReadOnly);
        CVPixelBufferRelease(frame);
        if (!readable) {
            copy_error(error, error_size, @"macOS could not read the screen picture's pixels");
            return -2;
        }
        return 1;
    }
}

void swarm_macos_screen_close(void *handle) {
    @autoreleasepool {
        SwarmScreenCapture *capture = (__bridge_transfer SwarmScreenCapture *)handle;
        stop_stream(capture.stream, capture.output);
        [capture.picker close];
    }
}
