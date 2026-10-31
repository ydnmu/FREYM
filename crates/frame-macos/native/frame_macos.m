// Apple SDK implementation of the narrow Rust media bridge. Minimum deployment: macOS 14.
#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#import <math.h>

typedef struct { void *surface; uint64_t pts_ns; uint32_t width, height, pixel_format; } FMFrame;
typedef struct { void *data; size_t length; uint64_t pts_ns, dts_ns, duration_ns; int keyframe; } FMPacket;
typedef struct { void *data; size_t count; uint64_t pts_ns; uint32_t rate; uint16_t channels; } FMAudioBlock;

static char *fm_string(NSString *value) { return strdup((value ?: @"Unknown native error").UTF8String); }
static uint64_t fm_ns(CMTime time) {
    if (!CMTIME_IS_VALID(time) || CMTIME_IS_INDEFINITE(time)) return UINT64_MAX;
    CMTime scaled = CMTimeConvertScale(time, 1000000000, kCMTimeRoundingMethod_RoundHalfAwayFromZero);
    return scaled.value < 0 ? UINT64_MAX : (uint64_t)scaled.value;
}
static NSString *fm_status(OSStatus status) { return [NSString stringWithFormat:@"Apple media API returned OSStatus %d", (int)status]; }

int fm_screen_authorized(void) { return CGPreflightScreenCaptureAccess() ? 1 : 0; }
int fm_request_screen(void) { return CGRequestScreenCaptureAccess() ? 1 : 0; }
uint64_t fm_host_time_ns(void) { return fm_ns(CMClockGetTime(CMClockGetHostTimeClock())); }
void fm_free(void *value) { free(value); }

static SCShareableContent *fm_content(char **error) {
    dispatch_semaphore_t ready = dispatch_semaphore_create(0);
    __block SCShareableContent *result = nil;
    __block NSError *failure = nil;
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO onScreenWindowsOnly:YES completionHandler:^(SCShareableContent *content, NSError *err) {
        result = content; failure = err; dispatch_semaphore_signal(ready);
    }];
    if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC))) {
        *error = fm_string(@"ScreenCaptureKit discovery timed out."); return nil;
    }
    if (!result) *error = fm_string(failure.localizedDescription);
    return result;
}

char *fm_discover(char **error) {
    @autoreleasepool {
        BOOL authorized = CGPreflightScreenCaptureAccess();
        SCShareableContent *content = authorized ? fm_content(error) : nil;
        if (authorized && !content) return NULL;
        NSMutableArray *displays = [NSMutableArray array];
        NSMutableArray *windows = [NSMutableArray array];
        NSMutableArray *apps = [NSMutableArray array];
        // CG display geometry is available before the first user-initiated permission request.
        // Once authorized, only SCShareableContent IDs are published as capturable targets.
        NSMutableArray<NSNumber *> *ids = [NSMutableArray array];
        if (authorized) {
            for (SCDisplay *display in content.displays) [ids addObject:@(display.displayID)];
        } else {
            uint32_t count = 0;
            CGGetActiveDisplayList(0, NULL, &count);
            CGDirectDisplayID *raw = calloc(count, sizeof(CGDirectDisplayID));
            if (raw) {
                CGGetActiveDisplayList(count, raw, &count);
                for (uint32_t i = 0; i < count; i++) [ids addObject:@(raw[i])];
                free(raw);
            }
        }
        for (NSNumber *number in ids) {
            CGDirectDisplayID display = number.unsignedIntValue;
            CGRect bounds = CGDisplayBounds(display);
            CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display);
            double rate = mode ? CGDisplayModeGetRefreshRate(mode) : 0;
            if (mode) CFRelease(mode);
            size_t pixelWidth = CGDisplayPixelsWide(display);
            size_t pixelHeight = CGDisplayPixelsHigh(display);
            double scale = bounds.size.width > 0 ? (double)pixelWidth / bounds.size.width : 1.0;
            [displays addObject:@{ @"id": number,
                @"name": [NSString stringWithFormat:@"Display %u", display],
                @"width": @(pixelWidth), @"height": @(pixelHeight),
                @"x": @((int)bounds.origin.x), @"y": @((int)bounds.origin.y),
                @"scale": @(scale), @"rate": rate > 0 ? @(rate) : [NSNull null],
                @"primary": @(display == CGMainDisplayID()) }];
        }
        if (authorized) {
            for (SCWindow *window in content.windows) {
                if (window.windowID == 0) continue;
                SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:window];
                double scale = filter.pointPixelScale;
                double pixelWidth = ceil(filter.contentRect.size.width * scale);
                double pixelHeight = ceil(filter.contentRect.size.height * scale);
                if (!isfinite(pixelWidth) || !isfinite(pixelHeight) || pixelWidth < 1 || pixelHeight < 1
                    || pixelWidth > UINT32_MAX || pixelHeight > UINT32_MAX) continue;
                uint32_t width = (uint32_t)pixelWidth;
                uint32_t height = (uint32_t)pixelHeight;
                [windows addObject:@{ @"id": @(window.windowID), @"title": window.title ?: @"Untitled window",
                    @"pid": @(window.owningApplication.processID),
                    @"width": @(width), @"height": @(height) }];
            }
            for (SCRunningApplication *app in content.applications) {
                if (!app.bundleIdentifier.length) continue;
                [apps addObject:@{ @"pid": @(app.processID), @"bundle": app.bundleIdentifier,
                    @"name": app.applicationName ?: app.bundleIdentifier }];
            }
        }
        NSMutableArray *microphones = [NSMutableArray array];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        for (AVCaptureDevice *device in [AVCaptureDevice devicesWithMediaType:AVMediaTypeAudio]) {
            [microphones addObject:@{ @"id": device.uniqueID, @"name": device.localizedName }];
        }
#pragma clang diagnostic pop
        NSDictionary *all = @{ @"screenAuthorized": @(authorized), @"displays": displays,
            @"windows": windows, @"applications": apps, @"microphones": microphones };
        NSData *json = [NSJSONSerialization dataWithJSONObject:all options:0 error:nil];
        if (!json) { *error = fm_string(@"Could not serialize native source discovery."); return NULL; }
        return strdup([[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding].UTF8String);
    }
}

@interface FMVideoCapture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(strong) SCStream *stream;
@property(strong) NSLock *lock;
@property(strong) NSMutableArray<NSValue *> *frames;
@property(strong) dispatch_queue_t queue;
@property(copy) NSString *failure;
@property(assign) uint32_t width, height;
@end
@implementation FMVideoCapture
- (instancetype)init { if ((self = [super init])) { _lock = [NSLock new]; _frames = [NSMutableArray array]; _queue = dispatch_queue_create("frame.macos.screen", DISPATCH_QUEUE_SERIAL); } return self; }
- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error { (void)stream; [_lock lock]; _failure = error.localizedDescription ?: @"Screen stream stopped."; [_lock unlock]; }
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sample ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen || !CMSampleBufferIsValid(sample)) return;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    if (!attachments || CFArrayGetCount(attachments) == 0) return;
    NSDictionary *frameInfo = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0);
    NSNumber *status = frameInfo[SCStreamFrameInfoStatus];
    if (!status || status.integerValue != SCFrameStatusComplete) return;
    CVImageBufferRef image = CMSampleBufferGetImageBuffer(sample);
    if (!image || fm_ns(CMSampleBufferGetPresentationTimeStamp(sample)) == UINT64_MAX) return;
    // Four retained IOSurfaces is the entire capture queue. A frame not admitted to the
    // encoder may be discarded here; an accepted encoder input is never discarded.
    [_lock lock];
    if (CVPixelBufferGetWidth(image) != _width || CVPixelBufferGetHeight(image) != _height
        || CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA) {
        _failure = @"ScreenCaptureKit changed the fixed BGRA surface geometry or pixel format.";
        [_lock unlock]; return;
    }
    if (_frames.count < 4 && !_failure) {
        FMFrame *frame = calloc(1, sizeof(FMFrame));
        if (frame) {
            CVPixelBufferRetain(image);
            frame->surface = image;
            frame->pts_ns = fm_ns(CMSampleBufferGetPresentationTimeStamp(sample));
            frame->width = (uint32_t)CVPixelBufferGetWidth(image);
            frame->height = (uint32_t)CVPixelBufferGetHeight(image);
            frame->pixel_format = CVPixelBufferGetPixelFormatType(image);
            [_frames addObject:[NSValue valueWithPointer:frame]];
        }
    }
    [_lock unlock];
}
- (void)dealloc { for (NSValue *value in _frames) { FMFrame *frame = value.pointerValue; CVPixelBufferRelease(frame->surface); free(frame); } }
@end

void *fm_capture_start(int target_kind, uint32_t target_id, uint32_t width, uint32_t height,
                       uint32_t rate_num, uint32_t rate_den, char **error) {
    @autoreleasepool {
        if (!width || !height || !rate_num || rate_num > INT32_MAX || !rate_den) {
            *error = fm_string(@"The fixed capture geometry or CMTime frame interval is invalid."); return NULL;
        }
        if (!CGPreflightScreenCaptureAccess() && !CGRequestScreenCaptureAccess()) {
            *error = fm_string(@"Screen recording permission was denied. Enable it in System Settings > Privacy & Security > Screen & System Audio Recording."); return NULL;
        }
        SCShareableContent *content = fm_content(error);
        if (!content) return NULL;
        SCContentFilter *filter = nil;
        if (target_kind == 0) {
            for (SCDisplay *item in content.displays) if (item.displayID == target_id)
                filter = [[SCContentFilter alloc] initWithDisplay:item excludingApplications:@[] exceptingWindows:@[]];
        } else if (target_kind == 1) {
            for (SCWindow *item in content.windows) if (item.windowID == target_id)
                filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:item];
        }
        if (!filter) { *error = fm_string(@"The selected ScreenCaptureKit target is no longer available."); return NULL; }
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = width; config.height = height;
        config.pixelFormat = kCVPixelFormatType_32BGRA;
        config.queueDepth = 4;
        config.showsCursor = YES;
        config.minimumFrameInterval = CMTimeMake(rate_den, rate_num);
        FMVideoCapture *owner = [FMVideoCapture new];
        owner.width = width; owner.height = height;
        owner.stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:owner];
        NSError *failure = nil;
        if (![owner.stream addStreamOutput:owner type:SCStreamOutputTypeScreen sampleHandlerQueue:owner.queue error:&failure]) {
            *error = fm_string(failure.localizedDescription); return NULL;
        }
        dispatch_semaphore_t ready = dispatch_semaphore_create(0);
        [owner.stream startCaptureWithCompletionHandler:^(NSError *err) { owner.failure = err.localizedDescription; dispatch_semaphore_signal(ready); }];
        if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC)) || owner.failure) {
            *error = fm_string(owner.failure ?: @"ScreenCaptureKit start timed out."); return NULL;
        }
        return (__bridge_retained void *)owner;
    }
}

int fm_capture_poll(void *raw, FMFrame *out, char **error) {
    FMVideoCapture *owner = (__bridge FMVideoCapture *)raw;
    [owner.lock lock];
    if (owner.failure) { *error = fm_string(owner.failure); [owner.lock unlock]; return -1; }
    if (!owner.frames.count) { [owner.lock unlock]; return 0; }
    FMFrame *frame = owner.frames.firstObject.pointerValue;
    [owner.frames removeObjectAtIndex:0];
    [owner.lock unlock];
    *out = *frame;
    free(frame);
    return 1;
}
void fm_surface_release(void *surface) { if (surface) CVPixelBufferRelease(surface); }
int fm_capture_stop(void *raw, char **error) {
    if (!raw) return 0;
    FMVideoCapture *owner = (__bridge_transfer FMVideoCapture *)raw;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block NSError *failure = nil;
    [owner.stream stopCaptureWithCompletionHandler:^(NSError *err) {
        // Retain the delegate until an asynchronous stop finishes, even if the wait times out.
        (void)owner.stream;
        failure = err; dispatch_semaphore_signal(done);
    }];
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC))) {
        *error = fm_string(@"ScreenCaptureKit stop timed out."); return -1;
    }
    dispatch_sync(owner.queue, ^{});
    if (failure) { *error = fm_string(failure.localizedDescription); return -1; }
    return 0;
}

@interface FMEncoder : NSObject
@property(assign) VTCompressionSessionRef session;
@property(strong) NSLock *lock;
@property(strong) NSMutableArray<NSValue *> *packets;
@property(strong) NSData *config;
@property(copy) NSString *failure;
@property(assign) NSUInteger inFlight;
@property(assign) BOOL draining;
@property(assign) uint32_t width, height;
@end
@implementation FMEncoder
- (instancetype)init { if ((self = [super init])) { _lock = [NSLock new]; _packets = [NSMutableArray array]; } return self; }
- (void)dealloc { if (_session) VTCompressionSessionInvalidate(_session); if (_session) CFRelease(_session); for (NSValue *value in _packets) { FMPacket *packet = value.pointerValue; free(packet->data); free(packet); } }
@end

static NSData *fm_annexb(CMSampleBufferRef sample, NSData **configuration) {
    CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
    size_t count = 0;
    int headerLength = 0;
    if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, &count, &headerLength)) return nil;
    NSMutableData *codec = [NSMutableData data];
    uint8_t start[] = {0, 0, 0, 1};
    for (size_t i = 0; i < count; i++) {
        const uint8_t *bytes = NULL; size_t length = 0;
        if (CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &bytes, &length, NULL, NULL)) return nil;
        if (!bytes || !length || length > 128 * 1024 || codec.length > 128 * 1024 - length
            || codec.length + length > 128 * 1024 - 4) return nil;
        [codec appendBytes:start length:4]; [codec appendBytes:bytes length:length];
    }
    *configuration = codec;
    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
    if (!block) return nil;
    size_t length = CMBlockBufferGetDataLength(block);
    NSMutableData *avcc = [NSMutableData dataWithLength:length];
    if (CMBlockBufferCopyDataBytes(block, 0, length, avcc.mutableBytes)) return nil;
    NSMutableData *annexb = [NSMutableData dataWithCapacity:length + 32];
    const uint8_t *bytes = avcc.bytes;
    for (size_t at = 0; at < length;) {
        if (headerLength < 1 || headerLength > 4 || length - at < (size_t)headerLength) return nil;
        uint32_t nal = 0;
        for (int i = 0; i < headerLength; i++) nal = (nal << 8) | bytes[at++];
        if (nal == 0 || nal > length - at) return nil;
        [annexb appendBytes:start length:4]; [annexb appendBytes:bytes + at length:nal]; at += nal;
    }
    return annexb;
}
static void fm_encoded(void *refcon, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample) {
    (void)source; (void)flags;
    FMEncoder *owner = (__bridge FMEncoder *)refcon;
    [owner.lock lock];
    if (owner.inFlight) owner.inFlight--;
    if (owner.failure) { [owner.lock unlock]; return; }
    if (status || !sample) { owner.failure = fm_status(status); [owner.lock unlock]; return; }
    CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(CMSampleBufferGetFormatDescription(sample));
    uint64_t pts = fm_ns(CMSampleBufferGetPresentationTimeStamp(sample));
    uint64_t dts = fm_ns(CMSampleBufferGetDecodeTimeStamp(sample));
    uint64_t duration = fm_ns(CMSampleBufferGetDuration(sample));
    if (dts == UINT64_MAX) dts = pts;
    if (dimensions.width != owner.width || dimensions.height != owner.height || pts > INT64_MAX
        || dts != pts || !duration || duration > INT64_MAX || pts > INT64_MAX - duration) {
        owner.failure = @"VideoToolbox output changed geometry or violated the fixed non-reordering timestamps.";
        [owner.lock unlock]; return;
    }
    NSData *config = nil;
    NSData *annexb = fm_annexb(sample, &config);
    if (!annexb || !config || annexb.length > 8 * 1024 * 1024 || owner.packets.count >= 8) {
        owner.failure = @"VideoToolbox output could not be admitted to the bounded packet queue.";
        [owner.lock unlock]; return;
    }
    if (owner.config && ![owner.config isEqualToData:config]) {
        owner.failure = @"VideoToolbox changed H.264 parameter sets; a new segment is required.";
        [owner.lock unlock]; return;
    }
    if (!owner.config) owner.config = config;
    FMPacket *packet = calloc(1, sizeof(FMPacket));
    if (!packet) { owner.failure = @"VideoToolbox packet allocation failed."; [owner.lock unlock]; return; }
    packet->data = malloc(annexb.length);
    if (!packet->data) { free(packet); owner.failure = @"VideoToolbox packet allocation failed."; [owner.lock unlock]; return; }
    memcpy(packet->data, annexb.bytes, annexb.length); packet->length = annexb.length;
    packet->pts_ns = pts; packet->dts_ns = dts; packet->duration_ns = duration;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    NSDictionary *entry = attachments && CFArrayGetCount(attachments) ? (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0) : nil;
    packet->keyframe = ![entry[(__bridge NSString *)kCMSampleAttachmentKey_NotSync] boolValue];
    [owner.packets addObject:[NSValue valueWithPointer:packet]];
    [owner.lock unlock];
}

void *fm_encoder_start(uint32_t width, uint32_t height, uint32_t rate_num, uint32_t rate_den,
                       uint32_t bitrate, uint32_t peak_bitrate, uint32_t gop, char **error) {
    FMEncoder *owner = [FMEncoder new];
    owner.width = width; owner.height = height;
    NSDictionary *spec = @{ (__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: @YES };
    VTCompressionSessionRef session = NULL;
    OSStatus status = VTCompressionSessionCreate(kCFAllocatorDefault, (int32_t)width, (int32_t)height,
        kCMVideoCodecType_H264, (__bridge CFDictionaryRef)spec, NULL, NULL, fm_encoded,
        (__bridge void *)owner, &session);
    if (status) { *error = fm_string(fm_status(status)); return NULL; }
    owner.session = session;
    status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel);
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef)@(bitrate));
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_DataRateLimits, (__bridge CFTypeRef)@[ @(peak_bitrate / 8), @1 ]);
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFTypeRef)@(gop));
    if (!status) status = VTSessionSetProperty(owner.session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFTypeRef)@((double)rate_num / rate_den));
    if (status) { *error = fm_string(fm_status(status)); return NULL; }
    status = VTCompressionSessionPrepareToEncodeFrames(owner.session);
    if (status) { *error = fm_string(fm_status(status)); return NULL; }
    return (__bridge_retained void *)owner;
}
int fm_encoder_submit(void *raw, void *surface, uint64_t pts_ns, uint64_t duration_ns, char **error) {
    FMEncoder *owner = (__bridge FMEncoder *)raw;
    [owner.lock lock];
    if (owner.failure || owner.draining) { *error = fm_string(owner.failure ?: @"Encoder admission is closed."); [owner.lock unlock]; return -1; }
    if (!surface || CVPixelBufferGetWidth(surface) != owner.width || CVPixelBufferGetHeight(surface) != owner.height
        || CVPixelBufferGetPixelFormatType(surface) != kCVPixelFormatType_32BGRA || pts_ns > INT64_MAX
        || !duration_ns || duration_ns > INT64_MAX || pts_ns > INT64_MAX - duration_ns) {
        owner.failure = @"The input surface or timestamp differs from the fixed VideoToolbox configuration.";
        *error = fm_string(owner.failure); [owner.lock unlock]; return -1;
    }
    if (owner.inFlight >= 8 || owner.packets.count >= 8) { [owner.lock unlock]; return 0; }
    owner.inFlight++;
    [owner.lock unlock];
    OSStatus status = VTCompressionSessionEncodeFrame(owner.session, surface,
        CMTimeMake((int64_t)pts_ns, 1000000000), CMTimeMake((int64_t)duration_ns, 1000000000),
        NULL, NULL, NULL);
    if (status) {
        [owner.lock lock]; owner.inFlight--; owner.failure = fm_status(status); [owner.lock unlock];
        *error = fm_string(fm_status(status)); return -1;
    }
    return 1;
}
int fm_encoder_poll(void *raw, FMPacket *out, char **error) {
    FMEncoder *owner = (__bridge FMEncoder *)raw;
    [owner.lock lock];
    if (owner.failure) { *error = fm_string(owner.failure); [owner.lock unlock]; return -1; }
    if (!owner.packets.count) { [owner.lock unlock]; return 0; }
    FMPacket *packet = owner.packets.firstObject.pointerValue;
    [owner.packets removeObjectAtIndex:0]; *out = *packet; free(packet);
    [owner.lock unlock]; return 1;
}
void *fm_encoder_config(void *raw, size_t *length) {
    FMEncoder *owner = (__bridge FMEncoder *)raw;
    [owner.lock lock]; NSData *data = owner.config;
    void *copy = data.length ? malloc(data.length) : NULL;
    if (copy) { memcpy(copy, data.bytes, data.length); *length = data.length; }
    [owner.lock unlock]; return copy;
}
int fm_encoder_drain(void *raw, char **error) {
    FMEncoder *owner = (__bridge FMEncoder *)raw;
    if (owner.draining) return 0;
    owner.draining = YES;
    OSStatus status = VTCompressionSessionCompleteFrames(owner.session, kCMTimeInvalid);
    if (status) { *error = fm_string(fm_status(status)); return -1; }
    return 0;
}
int fm_encoder_drained(void *raw) {
    FMEncoder *owner = (__bridge FMEncoder *)raw;
    [owner.lock lock]; int done = owner.draining && !owner.inFlight && !owner.packets.count && !owner.failure;
    [owner.lock unlock]; return done;
}
void fm_encoder_stop(void *raw) {
    if (!raw) return;
    FMEncoder *owner = (__bridge_transfer FMEncoder *)raw;
    VTCompressionSessionCompleteFrames(owner.session, kCMTimeInvalid);
    VTCompressionSessionInvalidate(owner.session);
}

@interface FMAudioCapture : NSObject <AVCaptureAudioDataOutputSampleBufferDelegate, SCStreamOutput, SCStreamDelegate>
@property(strong) AVCaptureSession *avSession;
@property(strong) AVCaptureDevice *audioDevice;
@property(strong) NSRunningApplication *audioApplication;
@property(strong) SCStream *scStream;
@property(strong) NSLock *lock;
@property(strong) NSMutableArray<NSValue *> *blocks;
@property(strong) dispatch_queue_t queue;
@property(copy) NSString *failure;
@property(assign) BOOL stopped;
@end
@implementation FMAudioCapture
- (instancetype)init { if ((self = [super init])) { _lock = [NSLock new]; _blocks = [NSMutableArray array]; _queue = dispatch_queue_create("frame.macos.audio", DISPATCH_QUEUE_SERIAL); } return self; }
- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error { (void)stream; [_lock lock]; _failure = error.localizedDescription; [_lock unlock]; }
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample fromConnection:(AVCaptureConnection *)connection {
    (void)output; (void)connection; [self accept:sample];
}
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sample ofType:(SCStreamOutputType)type {
    (void)stream; if (type == SCStreamOutputTypeAudio) [self accept:sample];
}
- (void)accept:(CMSampleBufferRef)sample {
    if (!CMSampleBufferIsValid(sample)) return;
    const AudioStreamBasicDescription *format = CMAudioFormatDescriptionGetStreamBasicDescription(CMSampleBufferGetFormatDescription(sample));
    if (!format || format->mFormatID != kAudioFormatLinearPCM || !(format->mFormatFlags & kAudioFormatFlagIsFloat)
        || format->mBitsPerChannel != 32 || format->mChannelsPerFrame == 0 || format->mChannelsPerFrame > 8) {
        [_lock lock]; _failure = @"Native audio is not float32 PCM; refusing an unconverted stream."; [_lock unlock]; return;
    }
    UInt32 channels = format->mChannelsPerFrame;
    CMBlockBufferRef retained = NULL;
    size_t listSize = offsetof(AudioBufferList, mBuffers) + channels * sizeof(AudioBuffer);
    AudioBufferList *list = calloc(1, listSize);
    if (!list) return;
    OSStatus status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample, &listSize, list,
        listSize, kCFAllocatorDefault, kCFAllocatorDefault, 0, &retained);
    if (status) { free(list); [_lock lock]; _failure = fm_status(status); [_lock unlock]; return; }
    CMItemCount frames = CMSampleBufferGetNumSamples(sample);
    if (frames <= 0 || frames > 4800) {
        CFRelease(retained); free(list);
        [_lock lock]; _failure = @"Native audio block exceeds the 4800-frame normalization limit."; [_lock unlock]; return;
    }
    size_t count = (size_t)frames * channels;
    FMAudioBlock *block = calloc(1, sizeof(FMAudioBlock));
    if (!block) { CFRelease(retained); free(list); return; }
    block->data = malloc(count * sizeof(float));
    if (!block->data) { free(block); CFRelease(retained); free(list); return; }
    float *out = block->data;
    if (list->mNumberBuffers == 1 && list->mBuffers[0].mDataByteSize >= count * sizeof(float)) {
        memcpy(out, list->mBuffers[0].mData, count * sizeof(float));
    } else if (list->mNumberBuffers == channels) {
        for (size_t channel = 0; channel < channels; channel++) {
            if (list->mBuffers[channel].mDataByteSize < (size_t)frames * sizeof(float)) {
                free(block->data); free(block); CFRelease(retained); free(list); return;
            }
            const float *src = list->mBuffers[channel].mData;
            for (size_t frame = 0; frame < (size_t)frames; frame++) out[frame * channels + channel] = src[frame];
        }
    } else { free(block->data); free(block); CFRelease(retained); free(list); return; }
    block->count = count; block->channels = (uint16_t)channels;
    block->rate = (uint32_t)format->mSampleRate;
    CMTime presentation = CMSampleBufferGetPresentationTimeStamp(sample);
    // AVCaptureSession timestamps use its synchronization clock; translate them to
    // the host clock sampled by the shared Rust ClockAnchor. ScreenCaptureKit audio
    // timestamps already use the host clock.
    if (self.avSession.synchronizationClock)
        presentation = CMSyncConvertTime(presentation, self.avSession.synchronizationClock, CMClockGetHostTimeClock());
    block->pts_ns = fm_ns(presentation);
    CFRelease(retained); free(list);
    if (block->pts_ns == UINT64_MAX || !block->rate) { free(block->data); free(block); return; }
    [_lock lock];
    if (_blocks.count >= 32) { _failure = @"Native audio exceeded its 32-block queue."; free(block->data); free(block); }
    else [_blocks addObject:[NSValue valueWithPointer:block]];
    [_lock unlock];
}
- (void)dealloc { for (NSValue *value in _blocks) { FMAudioBlock *block = value.pointerValue; free(block->data); free(block); } }
@end

void *fm_microphone_start(const char *device_id, char **error) {
    @autoreleasepool {
        AVAuthorizationStatus access = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
        if (access == AVAuthorizationStatusNotDetermined) {
            dispatch_semaphore_t done = dispatch_semaphore_create(0);
            __block BOOL granted = NO;
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL allowed) { granted = allowed; dispatch_semaphore_signal(done); }];
            if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC))) {
                *error = fm_string(@"Microphone permission request timed out."); return NULL;
            }
            if (!granted) { *error = fm_string(@"Microphone permission was denied."); return NULL; }
        } else if (access != AVAuthorizationStatusAuthorized) {
            *error = fm_string(@"Microphone permission was denied."); return NULL;
        }
        AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:[NSString stringWithUTF8String:device_id]];
        if (!device || ![device hasMediaType:AVMediaTypeAudio]) { *error = fm_string(@"Selected microphone is no longer available."); return NULL; }
        NSError *failure = nil;
        AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device error:&failure];
        if (!input) { *error = fm_string(failure.localizedDescription); return NULL; }
        FMAudioCapture *owner = [FMAudioCapture new];
        owner.audioDevice = device;
        owner.avSession = [AVCaptureSession new];
        AVCaptureAudioDataOutput *output = [AVCaptureAudioDataOutput new];
        output.audioSettings = @{ AVFormatIDKey: @(kAudioFormatLinearPCM), AVLinearPCMIsFloatKey: @YES,
            AVLinearPCMBitDepthKey: @32, AVLinearPCMIsNonInterleaved: @NO,
            AVSampleRateKey: @48000.0 };
        [output setSampleBufferDelegate:owner queue:owner.queue];
        if (![owner.avSession canAddInput:input] || ![owner.avSession canAddOutput:output]) {
            *error = fm_string(@"Selected microphone cannot attach to an AVCaptureSession."); return NULL;
        }
        [owner.avSession addInput:input]; [owner.avSession addOutput:output];
        [owner.avSession startRunning];
        if (!owner.avSession.isRunning) { *error = fm_string(@"Microphone capture did not start."); return NULL; }
        return (__bridge_retained void *)owner;
    }
}

void *fm_application_audio_start(uint32_t pid, uint32_t display_id, char **error) {
    @autoreleasepool {
        SCShareableContent *content = fm_content(error);
        if (!content) return NULL;
        SCDisplay *display = nil; SCRunningApplication *app = nil;
        for (SCDisplay *item in content.displays) if (item.displayID == display_id) display = item;
        for (SCRunningApplication *item in content.applications) if (item.processID == (pid_t)pid) app = item;
        if (!display || !app) { *error = fm_string(@"Selected application or display is no longer shareable."); return NULL; }
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display includingApplications:@[app] exceptingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = 2; config.height = 2; config.queueDepth = 3;
        config.capturesAudio = YES; config.excludesCurrentProcessAudio = NO;
        config.sampleRate = 48000; config.channelCount = 2;
        FMAudioCapture *owner = [FMAudioCapture new];
        owner.audioApplication = [NSRunningApplication runningApplicationWithProcessIdentifier:(pid_t)pid];
        if (!owner.audioApplication || owner.audioApplication.terminated) {
            *error = fm_string(@"Selected application audio source exited."); return NULL;
        }
        owner.scStream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:owner];
        NSError *failure = nil;
        if (![owner.scStream addStreamOutput:owner type:SCStreamOutputTypeAudio sampleHandlerQueue:owner.queue error:&failure]) {
            *error = fm_string(failure.localizedDescription); return NULL;
        }
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        [owner.scStream startCaptureWithCompletionHandler:^(NSError *err) { owner.failure = err.localizedDescription; dispatch_semaphore_signal(done); }];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC)) || owner.failure) {
            *error = fm_string(owner.failure ?: @"Application audio stream timed out."); return NULL;
        }
        return (__bridge_retained void *)owner;
    }
}
int fm_audio_poll(void *raw, FMAudioBlock *out, char **error) {
    FMAudioCapture *owner = (__bridge FMAudioCapture *)raw;
    [owner.lock lock];
    if (!owner.stopped && ((owner.audioDevice && !owner.audioDevice.connected)
        || (owner.audioApplication && owner.audioApplication.terminated)))
        owner.failure = @"Selected audio source exited or disconnected.";
    if (owner.failure) { *error = fm_string(owner.failure); [owner.lock unlock]; return -1; }
    if (!owner.blocks.count) { [owner.lock unlock]; return 0; }
    FMAudioBlock *block = owner.blocks.firstObject.pointerValue;
    [owner.blocks removeObjectAtIndex:0]; *out = *block; free(block);
    [owner.lock unlock]; return 1;
}
int fm_audio_request_drain(void *raw, char **error) {
    if (!raw) return 0;
    FMAudioCapture *owner = (__bridge FMAudioCapture *)raw;
    if (owner.stopped) return 0;
    owner.stopped = YES;
    if (owner.avSession) {
        [owner.avSession stopRunning];
        for (AVCaptureOutput *output in owner.avSession.outputs)
            if ([output isKindOfClass:[AVCaptureAudioDataOutput class]])
                [(AVCaptureAudioDataOutput *)output setSampleBufferDelegate:nil queue:NULL];
    }
    NSString *stopFailure = nil;
    if (owner.scStream) {
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block NSError *failure = nil;
        [owner.scStream stopCaptureWithCompletionHandler:^(NSError *err) {
            (void)owner.scStream;
            failure = err; dispatch_semaphore_signal(done);
        }];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC))) {
            stopFailure = @"Application audio stop timed out.";
        }
        if (failure) stopFailure = failure.localizedDescription;
    }
    dispatch_sync(owner.queue, ^{});
    if (stopFailure) { *error = fm_string(stopFailure); return -1; }
    return 0;
}
int fm_audio_stop(void *raw, char **error) {
    if (!raw) return 0;
    int status = fm_audio_request_drain(raw, error);
    FMAudioCapture *owner = (__bridge_transfer FMAudioCapture *)raw;
    (void)owner;
    return status;
}
