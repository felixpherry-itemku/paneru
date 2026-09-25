//! Live window streams and the desktop picture for the overview, captured with
//! `ScreenCaptureKit`.
//!
//! Stream frames go from `ScreenCaptureKit`'s queue straight to the main queue
//! and into the renderer's layers, never through ECS, so a settled overview
//! leaves Bevy idle. The desktop picture is one still capture, sent as an
//! event.
//!
//! Everything here is best effort: below macOS 14, without the Screen Recording
//! permission, or for a window `ScreenCaptureKit` does not know, nothing arrives
//! and the overview keeps drawing its icon + title tile, or its scrim-coloured
//! backdrop. Nothing ever errors.

// The stream half is unused between the Phase 1 probe's removal and Phase 4's
// renderer hookup; Phase 4 deletes this.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bevy::math::IRect;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGDirectDisplayID, CGImage};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_foundation::NSError;
use objc2_io_surface::IOSurfaceRef;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};
use tracing::debug;

use crate::events::{Event, EventSender};
use crate::manager::irect_from;
use crate::platform::WinID;
use crate::util::rgba_bitmap_context;

/// Called on the main thread with each new frame of one window's stream.
/// Built on the main thread by the renderer, which owns what it draws into.
pub type FrameSink = Arc<MainThreadBound<Box<dyn Fn(&IOSurfaceRef)>>>;

/// One window's live stream, running or still starting. Dropping it stops the
/// stream, or keeps a still-starting one from ever running. Never waits on
/// `ScreenCaptureKit`, so it is safe to drop on the main thread.
pub struct LiveStream(Arc<Mutex<Slot>>);

/// What a [`LiveStream`] shares with the completion handler that starts it.
#[derive(Default)]
struct Slot {
    /// Set by the handle's drop, so a completion arriving later starts nothing
    /// nobody owns.
    cancelled: bool,
    stream: Option<Retained<SCStream>>,
    /// Kept alive as long as the stream runs.
    output: Option<Retained<StreamOutput>>,
}

// SAFETY: `SCStream` isn't marked `Send`, but a slot only ever moves one
// between `ScreenCaptureKit`'s completion queue, which starts it, and the main
// thread, which stops it; both under the slot's mutex, never used at once.
unsafe impl Send for Slot {}

impl Drop for LiveStream {
    fn drop(&mut self) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        slot.cancelled = true;
        if let Some(stream) = slot.stream.take() {
            unsafe { stream.stopCaptureWithCompletionHandler(None) };
        }
        slot.output = None;
    }
}

/// An `IOSurface` on its way from `ScreenCaptureKit`'s sample queue to the
/// main thread.
struct SendSurface(CFRetained<IOSurfaceRef>);

// SAFETY: an `IOSurface` is a kernel object, safe to retain, release and use
// from any thread; this only moves one reference across a queue hop.
unsafe impl Send for SendSurface {}

define_class!(
    /// Receives one stream's samples and forwards each frame's surface to that
    /// window's [`FrameSink`].
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `StreamOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "PaneruStreamOutput"]
    #[ivars = FrameSink]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        /// Runs on [`stream_queue`] for every sample. Hands the frame's surface
        /// to the main queue and nothing else: no event, no waker, so a
        /// settled overview never ticks Bevy.
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle and blank samples carry no image: keep showing the last one.
            let Some(surface) = (unsafe { sample_buffer.image_buffer() })
                .and_then(|buffer| CVPixelBufferGetIOSurface(Some(&buffer)))
            else {
                return;
            };
            let surface = SendSurface(surface);
            let sink = self.ivars().clone();
            DispatchQueue::main().exec_async(move || {
                // Whole, so the closure captures the `Send` wrapper and not
                // its field.
                let surface = surface;
                if let Some(mtm) = MainThreadMarker::new() {
                    (sink.get(mtm))(&surface.0);
                }
            });
        }
    }
);

impl StreamOutput {
    fn new(sink: FrameSink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(sink);
        unsafe { msg_send![super(this), init] }
    }
}

/// The one serial queue every stream delivers its samples on.
fn stream_queue() -> &'static DispatchQueue {
    static QUEUE: OnceLock<DispatchRetained<DispatchQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| DispatchQueue::new("paneru.overview.streams", None))
}

/// Starts a live stream per `(window, pixel width, pixel height, sink)`, at
/// most 30 frames per second, with one shareable-content lookup for all of
/// them. Returns the handles at once; the streams come up asynchronously, and
/// a window `ScreenCaptureKit` can't see (or a denied permission) just never
/// delivers a frame. `ScreenCaptureKit` only sends a frame when the window's
/// content changes, so an idle stream costs next to nothing.
pub fn start_streams(requests: Vec<(WinID, u32, u32, FrameSink)>) -> Vec<(WinID, LiveStream)> {
    if requests.is_empty() || !objc2::available!(macos = 14.0) {
        return Vec::new();
    }
    let requests = requests
        .into_iter()
        .map(|(window_id, width, height, sink)| {
            let slot = Arc::new(Mutex::new(Slot::default()));
            (window_id, width, height, sink, slot)
        })
        .collect::<Vec<_>>();
    let live = requests
        .iter()
        .map(|(window_id, .., slot)| (*window_id, LiveStream(slot.clone())))
        .collect();

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // Null content is how a denied Screen Recording permission shows
            // up. Not an error worth surfacing: the tiles already have their
            // icon + title.
            let Some(content) = (unsafe { content.as_ref() }) else {
                debug!("no shareable content: {:?}", unsafe { error.as_ref() });
                return;
            };
            let by_id = unsafe { content.windows() }
                .iter()
                .map(|window| (unsafe { window.windowID() }, window))
                .collect::<HashMap<u32, Retained<SCWindow>>>();
            for &(window_id, width, height, ref sink, ref slot) in &requests {
                let Some(window) = u32::try_from(window_id).ok().and_then(|id| by_id.get(&id))
                else {
                    continue;
                };
                // Held until the stream is stored, so a drop racing this start
                // either cancels it first or finds it to stop.
                let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
                if slot.cancelled {
                    continue;
                }
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        window,
                    )
                };
                let config = configuration(width, height);
                unsafe {
                    config.setMinimumFrameInterval(CMTime {
                        value: 1,
                        timescale: 30,
                        flags: CMTimeFlags::Valid,
                        epoch: 0,
                    });
                }
                let output = StreamOutput::new(sink.clone());
                let stream = unsafe {
                    SCStream::initWithFilter_configuration_delegate(
                        SCStream::alloc(),
                        &filter,
                        &config,
                        None,
                    )
                };
                if let Err(error) = unsafe {
                    stream.addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output),
                        SCStreamOutputType::Screen,
                        Some(stream_queue()),
                    )
                } {
                    debug!("stream output for window {window_id}: {error:?}");
                    continue;
                }
                let started = RcBlock::new(move |error: *mut NSError| {
                    if let Some(error) = unsafe { error.as_ref() } {
                        debug!("stream start for window {window_id}: {error:?}");
                    }
                });
                unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
                slot.stream = Some(stream);
                slot.output = Some(output);
            }
        },
    );
    // `onScreenWindowsOnly` still covers every managed window, including those
    // the overview exists to reveal: a parked strip keeps `PARKED_STRIP_SLIVER`
    // on screen and a scrolled-away column keeps `sliver_width`, precisely so
    // macOS never treats them as hidden. That parking design is load-bearing
    // here — a window pushed fully off-display would silently lose its
    // stream.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &handler,
        );
    }
    live
}

/// Requests the desktop picture macOS is showing on `display` (absolute CG
/// points, menubar included) at `width` x `height` pixels, by capturing the
/// Dock's `Wallpaper-` window for it: the picture alone, without desktop icons,
/// widgets or windows. Returns immediately; the result arrives later as an
/// `Event::OverviewWallpaper`.
pub fn request_wallpaper(
    display_id: CGDirectDisplayID,
    display: IRect,
    width: u32,
    height: u32,
    events: EventSender,
) {
    if !objc2::available!(macos = 14.0) {
        return;
    }

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let Some(content) = (unsafe { content.as_ref() }) else {
                debug!("no shareable content: {:?}", unsafe { error.as_ref() });
                return;
            };
            let windows = unsafe { content.windows() };
            let Some(window) = windows.iter().find(|window| {
                let bundle_id = unsafe { window.owningApplication() }
                    .map(|app| unsafe { app.bundleIdentifier() }.to_string());
                let title = unsafe { window.title() }.map(|title| title.to_string());
                is_wallpaper(
                    bundle_id.as_deref().unwrap_or_default(),
                    title.as_deref().unwrap_or_default(),
                    irect_from(unsafe { window.frame() }),
                    display,
                )
            }) else {
                debug!("no wallpaper window for display {display_id}");
                return;
            };
            capture(
                &window,
                width,
                height,
                events.clone(),
                move |width, height, rgba| Event::OverviewWallpaper {
                    display_id,
                    width,
                    height,
                    rgba,
                },
            );
        },
    );
    // Desktop windows included: the wallpaper is one.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &handler,
        );
    }
}

/// Screenshots one window at `width` x `height` pixels, occluded or partly
/// off-screen as it may be, and sends the event `make` builds from the pixels.
fn capture(
    window: &SCWindow,
    width: u32,
    height: u32,
    events: EventSender,
    make: impl Fn(u32, u32, Vec<u8>) -> Event + 'static,
) {
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    let config = configuration(width, height);
    let handler = RcBlock::new(move |image: *mut CGImage, _error: *mut NSError| {
        // Flattened to plain bytes right here, on ScreenCaptureKit's queue:
        // a `CGImage` is not `Send`, bytes are, so nothing else has to be.
        let Some((width, height, rgba)) = (unsafe { image.as_ref() }).and_then(rgba_bytes) else {
            return;
        };
        _ = events.send(make(width, height, rgba));
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&handler),
        );
    }
}

/// One window at `width` x `height` pixels, without cursor or shadow.
fn configuration(width: u32, height: u32) -> Retained<SCStreamConfiguration> {
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setShowsCursor(false);
        config.setScalesToFit(true);
        config.setIgnoreShadowsSingleWindow(true);
    }
    config
}

/// Draws `image` into a fresh RGBA buffer.
#[allow(clippy::cast_precision_loss)]
fn rgba_bytes(image: &CGImage) -> Option<(u32, u32, Vec<u8>)> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    let context = unsafe { rgba_bitmap_context(rgba.as_mut_ptr().cast(), width, height) }?;
    let bounds = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
    CGContext::draw_image(Some(&context), bounds, Some(image));
    drop(context);
    Some((
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        rgba,
    ))
}

/// Whether a window is the Dock's desktop-picture window covering `display`
/// (absolute CG points, menubar included).
fn is_wallpaper(bundle_id: &str, title: &str, frame: IRect, display: IRect) -> bool {
    bundle_id == "com.apple.dock" && title.starts_with("Wallpaper") && frame == display
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wallpaper_picks_the_dock_window_covering_the_display() {
        let display = IRect::new(0, 0, 1512, 982);
        let other = IRect::new(1512, 0, 3432, 1080);
        assert!(is_wallpaper(
            "com.apple.dock",
            "Wallpaper-",
            display,
            display
        ));
        assert!(
            !is_wallpaper("com.apple.finder", "", display, display),
            "desktop icons"
        );
        assert!(
            !is_wallpaper(
                "com.apple.notificationcenterui",
                "Month",
                IRect::new(20, 40, 200, 220),
                display
            ),
            "a widget"
        );
        assert!(
            !is_wallpaper("com.apple.dock", "Wallpaper-", other, display),
            "another display's wallpaper"
        );
        assert!(
            !is_wallpaper("com.apple.dock", "Dock", display, display),
            "another Dock window"
        );
    }
}
