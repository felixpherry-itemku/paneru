//! Window thumbnails for the overview, captured with `ScreenCaptureKit`.
//!
//! Everything here is best effort: below macOS 14, without the Screen Recording
//! permission, or for a window `ScreenCaptureKit` does not know, nothing arrives
//! and the overview keeps drawing its icon + title tile. Nothing ever errors.

use std::collections::HashMap;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGImage};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};
use tracing::debug;

use crate::events::{Event, EventSender};
use crate::platform::WinID;
use crate::util::rgba_bitmap_context;

/// Requests a thumbnail per `(window, pixel width, pixel height)`. Returns
/// immediately; each result arrives later as an `Event::OverviewThumbnail`.
pub fn request_thumbnails(requests: Vec<(WinID, u32, u32)>, events: EventSender) {
    if requests.is_empty() || !objc2::available!(macos = 14.0) {
        return;
    }

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
            for &(window_id, width, height) in &requests {
                let Some(window) = u32::try_from(window_id).ok().and_then(|id| by_id.get(&id))
                else {
                    continue;
                };
                capture(window, window_id, width, height, events.clone());
            }
        },
    );
    // `onScreenWindowsOnly` still covers every managed window, including those
    // the overview exists to reveal: a parked strip keeps `PARKED_STRIP_SLIVER`
    // on screen and a scrolled-away column keeps `sliver_width`, precisely so
    // macOS never treats them as hidden. That parking design is load-bearing
    // here — a window pushed fully off-display would silently lose its
    // thumbnail.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &handler,
        );
    }
}

/// Screenshots one window at `width` x `height` pixels, occluded or partly
/// off-screen as it may be.
fn capture(window: &SCWindow, window_id: WinID, width: u32, height: u32, events: EventSender) {
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setShowsCursor(false);
        config.setScalesToFit(true);
        config.setIgnoreShadowsSingleWindow(true);
    }
    let handler = RcBlock::new(move |image: *mut CGImage, _error: *mut NSError| {
        // Flattened to plain bytes right here, on ScreenCaptureKit's queue:
        // a `CGImage` is not `Send`, bytes are, so nothing else has to be.
        let Some((width, height, rgba)) = (unsafe { image.as_ref() }).and_then(rgba_bytes) else {
            return;
        };
        _ = events.send(Event::OverviewThumbnail {
            window_id,
            width,
            height,
            rgba,
        });
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&handler),
        );
    }
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
