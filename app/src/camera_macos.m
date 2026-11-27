#import <AVFoundation/AVCaptureDevice.h>
#import <AVFoundation/AVCaptureInput.h>
#import <AVFoundation/AVCaptureSession.h>
#import <AVFoundation/AVCaptureVideoDataOutput.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <TargetConditionals.h>
#import <dispatch/dispatch.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

typedef void (*SwarmCameraDevice)(void *, const char *, const char *);
typedef void (*SwarmCameraFrame)(void *, const uint8_t *, size_t, size_t, size_t);

@interface SwarmCamera : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate> {
    CVPixelBufferRef _pending;
}
@property(nonatomic, strong) AVCaptureDevice *device;
@property(nonatomic, strong) AVCaptureSession *session;
@property(nonatomic, strong) AVCaptureVideoDataOutput *output;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, strong) NSCondition *ready;
- (CVPixelBufferRef)takeFor:(uint32_t)milliseconds CF_RETURNS_RETAINED;
@end

@implementation SwarmCamera
- (instancetype)init {
    if ((self = [super init])) {
        _session = [[AVCaptureSession alloc] init];
        _output = [[AVCaptureVideoDataOutput alloc] init];
        _queue = dispatch_queue_create("swarm.camera", DISPATCH_QUEUE_SERIAL);
        _ready = [[NSCondition alloc] init];
    }
    return self;
}

- (void)captureOutput:(AVCaptureOutput *)output
    didOutputSampleBuffer:(CMSampleBufferRef)sample
           fromConnection:(AVCaptureConnection *)connection {
    CVPixelBufferRef frame = CMSampleBufferGetImageBuffer(sample);
    if (!frame) {
        return;
    }
    CVPixelBufferRetain(frame);
    [_ready lock];
    CVPixelBufferRef previous = _pending;
    _pending = frame;
    [_ready signal];
    [_ready unlock];
    CVPixelBufferRelease(previous);
}

- (CVPixelBufferRef)takeFor:(uint32_t)milliseconds {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:milliseconds / 1000.0];
    [_ready lock];
    while (!_pending && [_ready waitUntilDate:deadline]) {
    }
    CVPixelBufferRef frame = _pending;
    _pending = NULL;
    [_ready unlock];
    return frame;
}

- (void)dealloc {
    CVPixelBufferRelease(_pending);
}
@end

void swarm_camera_devices(SwarmCameraDevice emit, void *context) {
    @autoreleasepool {
        NSArray<AVCaptureDeviceType> *types = @[
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            AVCaptureDeviceTypeExternal,
            AVCaptureDeviceTypeContinuityCamera,
        ];
        AVCaptureDeviceDiscoverySession *discovery = [AVCaptureDeviceDiscoverySession
            discoverySessionWithDeviceTypes:types mediaType:AVMediaTypeVideo
            position:AVCaptureDevicePositionUnspecified];
        AVCaptureDevice *defaultDevice = [AVCaptureDevice defaultDeviceWithMediaType:AVMediaTypeVideo];
        if (defaultDevice) {
            emit(context, defaultDevice.localizedName.UTF8String, defaultDevice.uniqueID.UTF8String);
        }
        for (AVCaptureDevice *device in discovery.devices) {
            if (![device.uniqueID isEqualToString:defaultDevice.uniqueID]) {
                emit(context, device.localizedName.UTF8String, device.uniqueID.UTF8String);
            }
        }
    }
}

// AVFoundation delivers the privacy response on a dispatch queue. This
// waits on Swarm's camera worker while the app's main thread keeps drawing.
int swarm_camera_authorize(void) {
    @autoreleasepool {
        if (![NSBundle.mainBundle objectForInfoDictionaryKey:@"NSCameraUsageDescription"]) {
            return -2;
        }
        AVAuthorizationStatus status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
        if (status == AVAuthorizationStatusAuthorized) {
            return 1;
        }
        if (status != AVAuthorizationStatusNotDetermined) {
            return 0;
        }
        dispatch_semaphore_t replied = dispatch_semaphore_create(0);
        __block BOOL allowed = NO;
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
            allowed = granted;
            dispatch_semaphore_signal(replied);
        }];
        if (dispatch_semaphore_wait(replied, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC)) != 0) {
            return -1;
        }
        return allowed;
    }
}

int32_t swarm_macos_microphone_permission(bool request) {
    @autoreleasepool {
        if (![NSBundle.mainBundle objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"]) {
            return -1;
        }
        AVAuthorizationStatus status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
        if (status == AVAuthorizationStatusAuthorized) {
            return 1;
        }
        if (status != AVAuthorizationStatusNotDetermined) {
            return 0;
        }
        static atomic_bool asking;
        if (request && !atomic_exchange(&asking, true)) {
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
                (void)granted;
                atomic_store(&asking, false);
            }];
        }
        return 2;
    }
}


typedef struct {
    CMTime duration;
    double rate;
    int64_t area;
    BOOL fits;
} SwarmCameraMode;

// Prefer a smooth mode within the requested size, or the smallest mode
// that works if the device only has larger modes.
static BOOL betterMode(SwarmCameraMode mode, SwarmCameraMode best) {
    if (mode.fits != best.fits) {
        return mode.fits;
    }
    BOOL smooth = mode.rate >= 25;
    BOOL bestSmooth = best.rate >= 25;
    if (smooth != bestSmooth) {
        return smooth;
    }
    if (mode.area != best.area) {
        return mode.fits ? mode.area > best.area : mode.area < best.area;
    }
    return mode.rate > best.rate;
}

static void configureDevice(AVCaptureDevice *device, uint32_t width, uint32_t height, uint32_t fps) {
    AVCaptureDeviceFormat *bestFormat = nil;
    SwarmCameraMode best = {0};
    for (AVCaptureDeviceFormat *format in device.formats) {
        CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format.formatDescription);
        if (size.width <= 0 || size.height <= 0 || size.width > 4096 || size.height > 4096) {
            continue;
        }
        SwarmCameraMode mode = {
            .area = (int64_t)size.width * size.height,
            .fits = (uint32_t)size.width <= width && (uint32_t)size.height <= height,
        };
        for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
            if (fps < range.minFrameRate) {
                continue;
            }
            double rate = MIN(fps, range.maxFrameRate);
            if (rate > mode.rate) {
                mode.rate = rate;
                mode.duration = fps >= range.maxFrameRate ? range.minFrameDuration : CMTimeMake(1, fps);
            }
        }
        if (mode.rate > 0 && (!bestFormat || betterMode(mode, best))) {
            best = mode;
            bestFormat = format;
        }
    }
    if (bestFormat && [device lockForConfiguration:nil]) {
        device.activeFormat = bestFormat;
        device.activeVideoMinFrameDuration = best.duration;
        device.activeVideoMaxFrameDuration = best.duration;
        [device unlockForConfiguration];
    }
}

static void *cameraError(char **error, NSString *message) {
    *error = strdup(message.UTF8String ?: "the camera could not start");
    return NULL;
}

static void closeCamera(SwarmCamera *camera) {
    [camera.output setSampleBufferDelegate:nil queue:NULL];
    [camera.session stopRunning];
    // Wait for any delegate call already queued before releasing its state.
    dispatch_sync(camera.queue, ^{});
}

void *swarm_camera_open(const char *id, uint32_t width, uint32_t height, uint32_t fps, char **error) {
    @autoreleasepool {
        AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:@(id)];
        if (!device) {
            return cameraError(error, @"the camera is no longer connected");
        }
        NSError *reason = nil;
        AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device error:&reason];
        if (!input) {
            return cameraError(error, reason.localizedDescription);
        }

        SwarmCamera *camera = [[SwarmCamera alloc] init];
        camera.device = device;
        if (![camera.session canAddInput:input] || ![camera.session canAddOutput:camera.output]) {
            return cameraError(error, @"the camera could not be connected to its capture session");
        }
        [camera.session beginConfiguration];
        [camera.session addInput:input];
        [camera.session addOutput:camera.output];
        configureDevice(device, width, height, fps);
        camera.output.videoSettings = @{ (__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA) };
        camera.output.alwaysDiscardsLateVideoFrames = YES;
        [camera.output setSampleBufferDelegate:camera queue:camera.queue];
        [camera.session commitConfiguration];
        [camera.session startRunning];
        if (!camera.session.running) {
            closeCamera(camera);
            return cameraError(error, @"the capture session would not start");
        }
        return (__bridge_retained void *)camera;
    }
}

// The frame is only lent to the synchronous callback while the pixels
// are locked. Return -1 if the stream has stopped, or 0 on a timeout.
int swarm_camera_take(void *handle, uint32_t milliseconds, SwarmCameraFrame emit, void *context) {
    @autoreleasepool {
        SwarmCamera *camera = (__bridge SwarmCamera *)handle;
        CVPixelBufferRef frame = [camera takeFor:milliseconds];
        if (!frame) {
            return camera.session.running && camera.device.connected ? 0 : -1;
        }
        int taken = 0;
        if (CVPixelBufferGetPixelFormatType(frame) == kCVPixelFormatType_32BGRA &&
            CVPixelBufferLockBaseAddress(frame, kCVPixelBufferLock_ReadOnly) == kCVReturnSuccess) {
            emit(context, CVPixelBufferGetBaseAddress(frame), CVPixelBufferGetWidth(frame),
                 CVPixelBufferGetHeight(frame), CVPixelBufferGetBytesPerRow(frame));
            CVPixelBufferUnlockBaseAddress(frame, kCVPixelBufferLock_ReadOnly);
            taken = 1;
        }
        CVPixelBufferRelease(frame);
        return taken;
    }
}

void swarm_camera_close(void *handle) {
    @autoreleasepool {
        SwarmCamera *camera = (__bridge_transfer SwarmCamera *)handle;
        closeCamera(camera);
    }
}
