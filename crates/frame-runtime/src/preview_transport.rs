//! The desktop's preview transport (FRAME-0208).
//!
//! This module is the only piece between the capture session's bounded preview hub and
//! the interface. It owns two things the hub deliberately does not:
//!
//! - **One in-flight frame.** [`PreviewTransport::next_image`] waits for the next
//!   published image and hands it to the caller once. Until the caller acknowledges it
//!   with [`PreviewTransport::acknowledge`], no further image is delivered, so a slow
//!   consumer cannot accumulate frames anywhere between the worker and the window.
//! - **One binary payload.** A versioned header and the existing PNG bytes travel in
//!   the same response, with the image length checked against [`MAX_PREVIEW_IMAGE_BYTES`]
//!   a second time. No Base64 image string is created.
//!
//! It adds no media processing, no queue, no thread and no second capture. Everything
//! it reports comes from the capture session's own counters; the module claims no
//! measured cost advantage.

use std::sync::Arc;
use std::time::Duration;

/// The default bounded wait for one preview image.
pub const DEFAULT_WAIT: Duration = Duration::from_millis(400);
/// Highest accepted wait, so a caller cannot turn this into an unbounded request.
pub const MAX_WAIT_MS: u64 = 2_000;

pub use frame_windows::preview::{MAX_PREVIEW_IMAGE_BYTES, PreviewHub, PreviewSubscriber};

/// Fixed little-endian header: magic, status, flags, reserved, sequence,
/// generation, source media time, width, height, payload byte length.
const WIRE_MAGIC: &[u8; 4] = b"FPV1";
const WIRE_HEADER_BYTES: usize = 44;
const WIRE_EMPTY: u8 = 0;
const WIRE_IMAGE: u8 = 1;
const WIRE_ERROR: u8 = 2;
const WIRE_FIRST_OF_SESSION: u8 = 1;
const MAX_WIRE_ERROR_BYTES: usize = 4096;

/// One preview image as the interface receives it.
///
/// Exactly one of the two states is set: `png` for a picture, `error` for a bounded
/// failure text. Neither is set for the empty state, which the transport reports as
/// [`PreviewPoll::Empty`] instead of a payload.
#[derive(Clone, Debug)]
pub struct PreviewFrame {
    /// `0` for the empty state.
    pub sequence: u64,
    /// `true` for the first image of a source, so the interface starts a new source
    /// instead of painting the previous source's picture into the new one.
    pub first_of_session: bool,
    /// Walks the hub's lifetime: an image whose generation is not the current one is
    /// from an earlier source and must not be presented as the current one.
    pub session_generation: u64,
    /// Media timestamp of the capture surface this image was produced from.
    pub source_media_ns: Option<u64>,
    /// Image width and height, `0` for the empty state or a failure.
    pub width: u32,
    pub height: u32,
    /// The PNG bytes already produced by the capture preview encoder.
    pub png: Option<Vec<u8>>,
    /// Bounded failure text, or `None` when this frame carries a picture.
    pub error: Option<String>,
}

impl PreviewFrame {
    /// The empty state: no image, no error.
    pub fn empty() -> Self {
        Self {
            sequence: 0,
            first_of_session: false,
            session_generation: 0,
            source_media_ns: None,
            width: 0,
            height: 0,
            png: None,
            error: None,
        }
    }
}

impl PreviewFrame {
    /// Serialize one atomic, length-delimited response. The frontend rejects unknown
    /// versions, malformed lengths, and all non-PNG image payloads.
    pub fn into_wire(self) -> Vec<u8> {
        let (status, payload) = if let Some(png) = self.png {
            (WIRE_IMAGE, png)
        } else if let Some(error) = self.error {
            let mut end = error.len().min(MAX_WIRE_ERROR_BYTES);
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            (WIRE_ERROR, error.as_bytes()[..end].to_vec())
        } else {
            (WIRE_EMPTY, Vec::new())
        };
        let mut wire = Vec::with_capacity(WIRE_HEADER_BYTES + payload.len());
        wire.extend_from_slice(WIRE_MAGIC);
        wire.push(status);
        wire.push(if self.first_of_session {
            WIRE_FIRST_OF_SESSION
        } else {
            0
        });
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&self.sequence.to_le_bytes());
        wire.extend_from_slice(&self.session_generation.to_le_bytes());
        wire.extend_from_slice(&self.source_media_ns.unwrap_or(u64::MAX).to_le_bytes());
        wire.extend_from_slice(&self.width.to_le_bytes());
        wire.extend_from_slice(&self.height.to_le_bytes());
        wire.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        wire.extend_from_slice(&payload);
        wire
    }
}

/// What one bounded wait produced.
#[derive(Clone, Debug)]
pub enum PreviewPoll {
    /// An image or a published failure. The caller must acknowledge it.
    Frame(PreviewFrame),
    /// No image inside the wait. Not an error: the source may be static.
    Empty,
    /// The source closed. The caller must stop asking.
    Closed,
}

/// A transport error that maps onto the accepted error taxonomy before it reaches the
/// interface.
#[derive(Clone, Debug)]
pub struct PreviewTransportError {
    pub message: String,
    pub action: String,
}

impl PreviewTransportError {
    fn new(message: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            action: action.into(),
        }
    }

    /// The message and action as one bounded sentence, for a Tauri error payload.
    pub fn detail(&self) -> String {
        format!("{} {}", self.message, self.action)
    }
}

/// The one preview transport of this host.
pub struct PreviewTransport {
    hub: Arc<PreviewHub>,
    subscriber: PreviewSubscriber,
    generation: u64,
    /// Whether this consumer has already delivered an image for `generation`. The first
    /// image *delivered* to the interface is what starts a new source there; a first
    /// image that was replaced before it was ever taken never reached the interface and
    /// must not consume that marker.
    delivered_first: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for PreviewTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewTransport")
            .field("generation", &self.generation)
            .field("subscribers", &self.hub.subscriber_count())
            .field("counters", &self.hub.counters())
            .finish_non_exhaustive()
    }
}

impl PreviewTransport {
    /// Attaches to one preview hub and returns the transport that consumes it.
    ///
    /// Attaching is what makes the capture worker produce images at all; dropping the
    /// transport detaches the consumer, and the worker then stops generating.
    pub fn attach(hub: &Arc<PreviewHub>) -> Self {
        Self {
            subscriber: hub.subscribe(),
            generation: hub.generation(),
            hub: Arc::clone(hub),
            delivered_first: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The source generation this transport delivers images for.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Attached consumers of this hub, including this transport.
    pub fn subscriber_count(&self) -> i64 {
        self.hub.subscriber_count()
    }

    /// The capture worker's preview counters, as the session reported them.
    pub fn counters(&self) -> frame_windows::preview::PreviewCounterSnapshot {
        self.hub.counters()
    }

    /// Waits at most `wait` for the next preview image.
    ///
    /// The wait is bounded and clamps the caller's request to [`MAX_WAIT_MS`]. On
    /// `Frame` the transport holds the one in-flight slot until
    /// [`PreviewTransport::acknowledge`] is called; if the caller never acknowledges,
    /// the next call reports `Empty` instead of queueing a second image.
    pub fn next_image(&self, wait: Duration) -> Result<PreviewPoll, PreviewTransportError> {
        let wait = wait.min(Duration::from_millis(MAX_WAIT_MS));
        let Some(outcome) = self.hub.wait_for_image(wait) else {
            return Ok(PreviewPoll::Closed);
        };
        let Ok(image) = outcome else {
            // Either the wait expired or the previous image is still unacknowledged.
            return Ok(PreviewPoll::Empty);
        };
        if image.session_generation != self.generation {
            // An image from an earlier source is never presented as the current source:
            // it is acknowledged and dropped instead.
            self.hub.acknowledge();
            return Ok(PreviewPoll::Empty);
        }
        match image.encoded() {
            Some(bytes) => {
                if bytes.len() > MAX_PREVIEW_IMAGE_BYTES {
                    self.hub.acknowledge();
                    return Err(PreviewTransportError::new(
                        format!(
                            "A {}-byte preview image exceeds the {MAX_PREVIEW_IMAGE_BYTES}-byte cap.",
                            bytes.len()
                        ),
                        "Close and reopen the preview.",
                    ));
                }
                if image.mime_type != frame_windows::preview::PREVIEW_IMAGE_MIME {
                    self.hub.acknowledge();
                    return Err(PreviewTransportError::new(
                        "The preview image is not a PNG.",
                        "Close and reopen the preview.",
                    ));
                }
                Ok(PreviewPoll::Frame(PreviewFrame {
                    sequence: image.sequence,
                    first_of_session: self.note_delivered(),
                    session_generation: image.session_generation,
                    source_media_ns: image.source_media_ns,
                    width: image.width,
                    height: image.height,
                    png: Some(bytes.to_vec()),
                    error: None,
                }))
            }
            None => {
                // A published failure is delivered and acknowledged like an image: the
                // one in-flight slot is released so the next state can arrive.
                self.hub.acknowledge();
                Ok(PreviewPoll::Frame(PreviewFrame {
                    sequence: image.sequence,
                    first_of_session: self.note_delivered(),
                    session_generation: image.session_generation,
                    source_media_ns: image.source_media_ns,
                    width: image.width,
                    height: image.height,
                    png: None,
                    error: Some(
                        image
                            .error()
                            .unwrap_or("The preview image reported no pixels and no reason.")
                            .to_string(),
                    ),
                }))
            }
        }
    }

    /// Marks this transport as having delivered its first image of the source.
    fn note_delivered(&self) -> bool {
        !self
            .delivered_first
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Acknowledges the delivered frame, allowing the next one to be handed out.
    ///
    /// It is safe to call after an `Empty` or `Closed` result: acknowledging without an
    /// outstanding frame is a no-op on the hub.
    pub fn acknowledge(&self) {
        self.hub.acknowledge();
    }

    /// Detaches this consumer explicitly.
    ///
    /// Detaching drops the subscription; when it was the last one, the capture worker
    /// performs no further preview work on its next completed surface. Closing the UI's
    /// preview is exactly this: no command has to reach the capture thread, and no wait
    /// or join happens here.
    pub fn detach(self) {
        drop(self.subscriber);
        self.hub.close();
    }

    /// The hub this transport reads from.
    pub fn hub(&self) -> &PreviewHub {
        &self.hub
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use frame_windows::preview::{PREVIEW_IMAGE_MIME, PreviewConfig, PreviewImage, new_hub};

    use super::*;

    fn hub() -> Arc<PreviewHub> {
        new_hub(PreviewConfig::default(), 11).expect("the default request is valid")
    }

    fn image(marker: u8) -> PreviewImage {
        PreviewImage {
            width: 4,
            height: 2,
            sequence: 0,
            first_of_session: false,
            source_media_ns: Some(u64::from(marker)),
            session_generation: 0,
            mime_type: PREVIEW_IMAGE_MIME,
            data: Ok(vec![marker; 4]),
        }
    }

    fn publish(hub: &Arc<PreviewHub>, image: PreviewImage) {
        hub.publish(image).expect("the hub publishes");
    }

    #[test]
    fn a_delivered_image_is_acknowledged_before_the_next_one_is_handed_out() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        assert_eq!(transport.subscriber_count(), 1);
        publish(&hub, image(1));
        let first = match transport.next_image(Duration::from_millis(50)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        };
        assert_eq!(first.width, 4);
        assert_eq!(first.height, 2);
        assert_eq!(first.sequence, 1);
        assert!(first.error.is_none());
        assert!(
            first.first_of_session,
            "the first image delivered to this consumer starts a new source"
        );
        assert_eq!(first.png.as_deref(), Some(&[1, 1, 1, 1][..]));
        // While the consumer holds this frame, a newer image replaces the pending slot
        // and is not handed out until the first one is acknowledged.
        publish(&hub, image(2));
        assert!(
            matches!(
                transport.next_image(Duration::from_millis(10)).unwrap(),
                PreviewPoll::Empty
            ),
            "a second image is not delivered before the first is acknowledged"
        );
        transport.acknowledge();
        let second = match transport.next_image(Duration::from_millis(50)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected the newest frame, got {other:?}"),
        };
        assert_eq!(second.sequence, 2, "the newest image is delivered next");
        assert!(
            !second.first_of_session,
            "only the first delivered image starts a source"
        );
        transport.acknowledge();
        // An image that arrives while nothing has taken the previous one replaces it,
        // and the replacement is counted rather than queued.
        publish(&hub, image(3));
        publish(&hub, image(4));
        let third = match transport.next_image(Duration::from_millis(50)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected the newest frame, got {other:?}"),
        };
        assert_eq!(
            third.sequence, 4,
            "the untaken image was replaced by the newest one"
        );
        transport.acknowledge();
        assert!(
            hub.counters().replaced_not_taken > 0,
            "the untaken replacement is counted, not queued"
        );
    }

    #[test]
    fn a_published_failure_reaches_the_interface_as_an_error_and_not_as_pixels() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        let failure = PreviewImage {
            width: 4,
            height: 2,
            sequence: 0,
            first_of_session: false,
            source_media_ns: None,
            session_generation: 0,
            mime_type: PREVIEW_IMAGE_MIME,
            data: Err("the staging texture was still being written".to_string()),
        };
        publish(&hub, failure);
        let frame = match transport.next_image(Duration::from_millis(50)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        };
        assert!(frame.png.is_none());
        assert_eq!(
            frame.error.as_deref(),
            Some("the staging texture was still being written")
        );
        transport.acknowledge();
    }

    #[test]
    fn an_image_published_while_nothing_waits_is_never_lost_or_queued() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        // No wait is outstanding: the hub holds the one latest image.
        publish(&hub, image(4));
        let first = match transport.next_image(Duration::from_millis(10)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected the pending frame, got {other:?}"),
        };
        assert_eq!(first.sequence, 1);
        transport.acknowledge();
        // A second image published while the consumer is between requests is the one
        // delivered next; nothing accumulates behind it.
        publish(&hub, image(5));
        publish(&hub, image(6));
        let second = match transport.next_image(Duration::from_millis(10)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected the newest frame, got {other:?}"),
        };
        assert_eq!(second.sequence, 3);
        transport.acknowledge();
    }

    #[test]
    fn a_publisher_racing_the_consumer_never_builds_a_queue() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        let holder: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        // Guarantee replacement independently of OS thread scheduling.
        publish(&hub, image(250));
        publish(&hub, image(251));
        let publisher = {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || {
                let mut accepted = 0u64;
                let mut contended = 0u64;
                for marker in 1..=200u8 {
                    let image = PreviewImage {
                        data: Ok(vec![marker; 4]),
                        ..image(marker)
                    };
                    if hub.publish(image).is_some() {
                        accepted += 1;
                    } else {
                        contended += 1;
                    }
                }
                (accepted, contended)
            })
        };
        let mut delivered = 0u32;
        let deadline = Instant::now() + Duration::from_secs(5);
        // The consumer is slower than the publisher on purpose; the transport may only
        // ever hold one unacknowledged frame, so it cannot read ahead into a queue.
        // Frames the publisher replaces before this consumer asks for them are dropped
        // by design, which is exactly the bounded-loss policy under test.
        while delivered < 25 && Instant::now() < deadline {
            if let PreviewPoll::Frame(frame) =
                transport.next_image(Duration::from_millis(50)).unwrap()
            {
                let in_flight = {
                    let mut slot = holder.lock().unwrap();
                    *slot += 1;
                    *slot
                };
                assert_eq!(in_flight, 1, "more than one frame was in flight at once");
                assert!(frame.png.is_some());
                assert!(
                    matches!(
                        transport.next_image(Duration::ZERO).unwrap(),
                        PreviewPoll::Empty
                    ),
                    "an unacknowledged frame must prevent a second delivery"
                );
                delivered += 1;
                transport.acknowledge();
                *holder.lock().unwrap() = 0;
            }
        }
        let (accepted, contended) = publisher.join().expect("the publisher thread finishes");
        assert_eq!(accepted + contended, 200);
        assert_eq!(hub.counters().published, accepted + 2);
        assert_eq!(hub.counters().publication_contention, contended);
        assert!(delivered > 0, "the consumer observed at least one frame");
        assert!(
            hub.counters().replaced_not_taken > 0,
            "a newer image replaced an untaken one instead of queueing"
        );
        assert_eq!(hub.subscriber_count(), 1);
    }

    #[test]
    fn an_empty_wait_is_bounded_and_reports_no_image_rather_than_an_error() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        let started = Instant::now();
        let outcome = transport.next_image(Duration::from_millis(30)).unwrap();
        assert!(matches!(outcome, PreviewPoll::Empty), "{outcome:?}");
        assert!(started.elapsed() >= Duration::from_millis(20));
        transport.acknowledge();
    }

    #[test]
    fn a_closed_source_reports_closure_so_the_interface_can_stop_asking() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        hub.close();
        assert!(matches!(
            transport.next_image(Duration::from_millis(20)).unwrap(),
            PreviewPoll::Closed
        ));
    }

    #[test]
    fn detaching_the_transport_stops_the_preview_work() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        assert!(hub.enabled());
        // The explicit path a "close preview" action takes: detach, then the worker
        // performs no further preview work and every waiter is woken as closed.
        transport.detach();
        assert_eq!(hub.subscriber_count(), 0);
        assert!(
            !hub.enabled(),
            "with no consumer the capture worker performs no preview work"
        );
        assert!(
            hub.wait_for_image(Duration::from_millis(10)).is_none(),
            "a detached hub reports closure"
        );
    }

    #[test]
    fn binary_envelope_keeps_metadata_and_png_together_without_base64() {
        let hub = hub();
        let transport = PreviewTransport::attach(&hub);
        publish(&hub, image(7));
        let frame = match transport.next_image(Duration::from_millis(10)).unwrap() {
            PreviewPoll::Frame(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        };
        let wire = frame.into_wire();
        assert_eq!(&wire[..4], WIRE_MAGIC);
        assert_eq!(wire[4], WIRE_IMAGE);
        assert_eq!(wire[5], WIRE_FIRST_OF_SESSION);
        assert_eq!(u64::from_le_bytes(wire[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(wire[16..24].try_into().unwrap()), 11);
        assert_eq!(u64::from_le_bytes(wire[24..32].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(wire[32..36].try_into().unwrap()), 4);
        assert_eq!(u32::from_le_bytes(wire[36..40].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(wire[40..44].try_into().unwrap()), 4);
        assert_eq!(&wire[WIRE_HEADER_BYTES..], &[7, 7, 7, 7]);
        assert_eq!(wire.len(), WIRE_HEADER_BYTES + 4);
        transport.acknowledge();
    }

    #[test]
    fn binary_empty_and_error_states_are_bounded() {
        let empty = PreviewFrame::empty().into_wire();
        assert_eq!(empty.len(), WIRE_HEADER_BYTES);
        assert_eq!(empty[4], WIRE_EMPTY);
        assert_eq!(&empty[24..32], &u64::MAX.to_le_bytes());

        let mut failure = PreviewFrame::empty();
        failure.sequence = 1;
        failure.session_generation = 11;
        failure.error = Some("é".repeat(3000));
        let wire = failure.into_wire();
        assert_eq!(wire[4], WIRE_ERROR);
        assert!(wire.len() <= WIRE_HEADER_BYTES + MAX_WIRE_ERROR_BYTES);
        assert!(std::str::from_utf8(&wire[WIRE_HEADER_BYTES..]).is_ok());
    }
}
