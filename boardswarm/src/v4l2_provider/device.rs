use crate::{Media, MediaError, MediaSignalMsg, MediaSignallingRx};

#[cfg(feature = "gstreamer")]
use {
    futures::stream::BoxStream,
    gstreamer::prelude::*,
    gstreamer_webrtc::{WebRTCSDPType, WebRTCSessionDescription},
    std::collections::HashMap,
    std::pin::Pin,
    std::sync::{Arc, Mutex},
    std::task::{Context, Poll},
    tokio::sync::mpsc,
    tracing::{info, warn},
};

// ---------------------------------------------------------------------------
// GStreamer helper functions (only compiled with the gstreamer feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
fn select_element(
    klass_terms: &[&str],
    caps: &gstreamer::Caps,
    is_sink: bool,
    fallback: &str,
    label: &str,
) -> String {
    let registry = gstreamer::Registry::get();
    let factories: Vec<gstreamer::ElementFactory> = registry
        .features_filtered(
            |feature| {
                let Some(f) = feature.downcast_ref::<gstreamer::ElementFactory>() else {
                    return false;
                };
                let klass = f.metadata("klass").unwrap_or_default();
                if !klass_terms.iter().all(|t| klass.contains(t)) {
                    return false;
                }
                if is_sink {
                    f.can_sink_any_caps(caps)
                } else {
                    f.can_src_any_caps(caps)
                }
            },
            false,
        )
        .into_iter()
        .filter_map(|f| f.downcast::<gstreamer::ElementFactory>().ok())
        .collect();

    let mut hw: Vec<&gstreamer::ElementFactory> = Vec::new();
    let mut sw: Vec<&gstreamer::ElementFactory> = Vec::new();
    for f in &factories {
        let klass = f.metadata("klass").unwrap_or_default();
        if klass.contains("Hardware") {
            hw.push(f);
        } else {
            sw.push(f);
        }
    }
    hw.sort_by_key(|f| std::cmp::Reverse(f.rank()));
    sw.sort_by_key(|f| std::cmp::Reverse(f.rank()));

    for factory in hw.iter().chain(sw.iter()) {
        let name = factory.name();
        if factory.create().build().is_ok() {
            tracing::debug!("{label}: selected {name}");
            return name.to_string();
        }
    }

    tracing::warn!("No {label} found; falling back to {fallback}");
    fallback.to_string()
}

#[cfg(feature = "gstreamer")]
fn select_h264_encoder() -> String {
    select_element(
        &["Encoder", "Video"],
        &gstreamer::Caps::builder("video/x-h264").build(),
        false,
        "x264enc",
        "H.264 encoder",
    )
}

#[cfg(feature = "gstreamer")]
fn select_jpeg_decoder() -> String {
    select_element(
        &["Decoder", "Image"],
        &gstreamer::Caps::builder("image/jpeg").build(),
        true,
        "jpegdec",
        "JPEG decoder",
    )
}

#[cfg(feature = "gstreamer")]
fn device_supports_mjpeg(device: &str) -> bool {
    let Ok(src) = gstreamer::ElementFactory::make("v4l2src")
        .property("device", device)
        .build()
    else {
        return false;
    };
    if src.set_state(gstreamer::State::Ready).is_err() {
        return false;
    }
    let supported = src
        .static_pad("src")
        .map(|pad| {
            pad.query_caps(None)
                .iter()
                .any(|s: &gstreamer::StructureRef| s.name() == "image/jpeg")
        })
        .unwrap_or(false);
    let _ = src.set_state(gstreamer::State::Null);
    supported
}

#[cfg(feature = "gstreamer")]
fn build_jpeg_decoder_bin(decoder_factory: &str) -> gstreamer::Bin {
    let bin = gstreamer::Bin::new();
    let parse = gstreamer::ElementFactory::make("jpegparse")
        .build()
        .expect("jpegparse");
    let decode = gstreamer::ElementFactory::make(decoder_factory)
        .build()
        .unwrap_or_else(|_| panic!("Failed to create JPEG decoder '{decoder_factory}'"));
    bin.add_many([&parse, &decode]).unwrap();
    gstreamer::Element::link_many([&parse, &decode]).unwrap();
    let sink_pad = parse.static_pad("sink").unwrap();
    bin.add_pad(&gstreamer::GhostPad::with_target(&sink_pad).unwrap())
        .unwrap();
    let src_pad = decode.static_pad("src").unwrap();
    bin.add_pad(&gstreamer::GhostPad::with_target(&src_pad).unwrap())
        .unwrap();
    bin
}

// ---------------------------------------------------------------------------
// Per-viewer GStreamer state (only compiled with the gstreamer feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
type ViewerId = u64;

#[cfg(feature = "gstreamer")]
struct ViewerState {
    webrtcbin: gstreamer::Element,
    tee_src_pad: gstreamer::Pad,
    queue_sink_pad: gstreamer::Pad,
}

/// Shared pipeline state for all concurrent viewers of a single V4L2 device.
#[cfg(feature = "gstreamer")]
struct V4l2DeviceInner {
    pipeline: gstreamer::Pipeline,
    tee: gstreamer::Element,
    encoder_factory: String,
    next_id: ViewerId,
    viewers: HashMap<ViewerId, ViewerState>,
}

#[cfg(feature = "gstreamer")]
impl std::fmt::Debug for V4l2DeviceInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2DeviceInner")
            .field("encoder_factory", &self.encoder_factory)
            .field("viewer_count", &self.viewers.len())
            .finish()
    }
}

#[cfg(feature = "gstreamer")]
impl V4l2DeviceInner {
    fn build(device_path: &str) -> Self {
        let encoder_factory = select_h264_encoder();
        let use_mjpeg = device_supports_mjpeg(device_path);
        if use_mjpeg {
            info!("V4L2 device {device_path}: using MJPEG input");
        }

        let pipeline = gstreamer::Pipeline::new();

        let tee = if use_mjpeg {
            let src = gstreamer::ElementFactory::make("v4l2src")
                .property("device", device_path)
                .build()
                .expect("v4l2src");
            // Constrain to MJPEG format but let the device pick resolution/fps
            let jpeg_caps = gstreamer::Caps::builder("image/jpeg").build();
            let capsfilter = gstreamer::ElementFactory::make("capsfilter")
                .property("caps", &jpeg_caps)
                .build()
                .expect("capsfilter (jpeg)");
            let jpeg_decoder_factory = select_jpeg_decoder();
            let decoder_bin = build_jpeg_decoder_bin(&jpeg_decoder_factory);
            let videoconvert = gstreamer::ElementFactory::make("videoconvert")
                .build()
                .expect("videoconvert");
            let tee = gstreamer::ElementFactory::make("tee")
                .property("allow-not-linked", true)
                .build()
                .expect("tee");
            pipeline
                .add_many([
                    &src,
                    &capsfilter,
                    decoder_bin.upcast_ref::<gstreamer::Element>(),
                    &videoconvert,
                    &tee,
                ])
                .expect("add MJPEG pipeline elements");
            gstreamer::Element::link_many([
                &src,
                &capsfilter,
                decoder_bin.upcast_ref::<gstreamer::Element>(),
                &videoconvert,
                &tee,
            ])
            .expect("link MJPEG pipeline elements");
            tee
        } else {
            // Raw video path — let v4l2src negotiate its preferred format freely
            let src = gstreamer::ElementFactory::make("v4l2src")
                .property("device", device_path)
                .build()
                .expect("v4l2src");
            let videoconvert = gstreamer::ElementFactory::make("videoconvert")
                .build()
                .expect("videoconvert");
            let tee = gstreamer::ElementFactory::make("tee")
                .property("allow-not-linked", true)
                .build()
                .expect("tee");
            pipeline
                .add_many([&src, &videoconvert, &tee])
                .expect("add raw pipeline elements");
            gstreamer::Element::link_many([&src, &videoconvert, &tee])
                .expect("link raw pipeline elements");
            tee
        };

        // Spawn a thread to monitor the pipeline bus for errors / EOS
        let bus = pipeline.bus().expect("Pipeline has no bus");
        let pipeline_weak = pipeline.downgrade();
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                match msg.view() {
                    gstreamer::MessageView::Eos(..) => {
                        warn!("V4L2 pipeline: end of stream");
                        break;
                    }
                    gstreamer::MessageView::Error(err) => {
                        warn!(
                            "V4L2 pipeline error from {:?}: {}",
                            err.src().map(|s| s.path_string()),
                            err.error()
                        );
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(p) = pipeline_weak.upgrade() {
                let _ = p.set_state(gstreamer::State::Null);
            }
        });

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("Failed to start V4L2 pipeline");

        V4l2DeviceInner {
            pipeline,
            tee,
            encoder_factory,
            next_id: 0,
            viewers: HashMap::new(),
        }
    }

    fn add_viewer(&mut self, tx: mpsc::UnboundedSender<MediaSignalMsg>) -> ViewerId {
        let id = self.next_id;
        self.next_id += 1;

        let queue = gstreamer::ElementFactory::make("queue")
            .name(format!("queue-{id}"))
            .property("max-size-time", 200_000_000u64)
            .build()
            .expect("queue");
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .name(format!("convert-{id}"))
            .build()
            .expect("videoconvert");
        let encoder = gstreamer::ElementFactory::make(&self.encoder_factory)
            .name(format!("enc-{id}"))
            .build()
            .unwrap_or_else(|_| panic!("Failed to create encoder '{}'", self.encoder_factory));

        if encoder.find_property("bitrate").is_some() {
            encoder.set_property("bitrate", 2000u32);
        }
        if encoder.find_property("tune").is_some() {
            encoder.set_property_from_str("tune", "zerolatency");
        }
        if encoder.find_property("speed-preset").is_some() {
            encoder.set_property_from_str("speed-preset", "ultrafast");
        }
        if encoder.find_property("key-int-max").is_some() {
            encoder.set_property("key-int-max", 60u32);
        }

        let h264parse = gstreamer::ElementFactory::make("h264parse")
            .name(format!("parse-{id}"))
            .build()
            .expect("h264parse");
        let pay = gstreamer::ElementFactory::make("rtph264pay")
            .name(format!("pay-{id}"))
            .property_from_str("config-interval", "-1")
            .property_from_str("aggregate-mode", "zero-latency")
            .build()
            .expect("rtph264pay");
        let rtp_caps = gstreamer::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .field("payload", 96i32)
            .build();
        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .name(format!("capsfilter-{id}"))
            .property("caps", &rtp_caps)
            .build()
            .expect("capsfilter");
        let webrtcbin = gstreamer::ElementFactory::make("webrtcbin")
            .name(format!("webrtc-{id}"))
            .property_from_str("bundle-policy", "max-bundle")
            .property("stun-server", "stun://stun.l.google.com:19302")
            .build()
            .expect("webrtcbin");

        self.pipeline
            .add_many([
                &queue, &convert, &encoder, &h264parse, &pay, &capsfilter, &webrtcbin,
            ])
            .unwrap();
        gstreamer::Element::link_many([&queue, &convert, &encoder, &h264parse, &pay, &capsfilter])
            .unwrap();

        let webrtc_sink = webrtcbin
            .request_pad_simple("sink_%u")
            .expect("webrtcbin sink pad");
        capsfilter
            .static_pad("src")
            .unwrap()
            .link(&webrtc_sink)
            .expect("capsfilter → webrtcbin");

        let tee_src_pad = self.tee.request_pad_simple("src_%u").expect("tee src pad");
        let queue_sink_pad = queue.static_pad("sink").unwrap();
        tee_src_pad.link(&queue_sink_pad).expect("tee → queue");

        for el in [
            &queue, &convert, &encoder, &h264parse, &pay, &capsfilter, &webrtcbin,
        ] {
            el.sync_state_with_parent().unwrap();
        }

        // Connect on-negotiation-needed: create offer and send to the client
        let tx_offer = tx.clone();
        webrtcbin.connect("on-negotiation-needed", false, move |values| {
            let webrtcbin = values[0].get::<gstreamer::Element>().unwrap();
            let tx = tx_offer.clone();
            let webrtcbin_promise = webrtcbin.clone();
            let promise = gstreamer::Promise::with_change_func(move |reply| {
                let Ok(Some(reply)) = reply else {
                    return;
                };
                let offer = reply.get::<WebRTCSessionDescription>("offer").unwrap();
                webrtcbin_promise.emit_by_name::<()>(
                    "set-local-description",
                    &[&offer, &None::<gstreamer::Promise>],
                );
                let sdp_text = offer.sdp().as_text().unwrap();
                let _ = tx.send(MediaSignalMsg::Offer(sdp_text));
            });
            webrtcbin
                .emit_by_name::<()>("create-offer", &[&None::<gstreamer::Structure>, &promise]);
            None
        });

        // Forward ICE candidates to the client
        webrtcbin.connect("on-ice-candidate", false, move |values| {
            let mline_index = values[1].get::<u32>().unwrap();
            let candidate = values[2].get::<String>().unwrap();
            let _ = tx.send(MediaSignalMsg::Ice(candidate, mline_index));
            None
        });

        self.viewers.insert(
            id,
            ViewerState {
                webrtcbin,
                tee_src_pad,
                queue_sink_pad,
            },
        );
        info!("V4L2: added viewer {id} (encoder: {})", self.encoder_factory);
        id
    }

    fn remove_viewer(&mut self, id: ViewerId) {
        let Some(viewer) = self.viewers.remove(&id) else {
            return;
        };
        let _ = viewer.tee_src_pad.unlink(&viewer.queue_sink_pad);
        self.tee.release_request_pad(&viewer.tee_src_pad);

        let branch_elements: Vec<gstreamer::Element> =
            ["queue", "convert", "enc", "parse", "pay", "capsfilter", "webrtc"]
                .iter()
                .filter_map(|prefix| self.pipeline.by_name(&format!("{prefix}-{id}")))
                .collect();
        for el in &branch_elements {
            el.set_state(gstreamer::State::Null).unwrap();
            self.pipeline.remove(el).unwrap();
        }
        info!("V4L2: removed viewer {id}");
    }
}

// ---------------------------------------------------------------------------
// GstMediaSignalRx — feeds incoming client signals into the webrtcbin
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
struct GstMediaSignalRx {
    webrtcbin: gstreamer::Element,
}

#[cfg(feature = "gstreamer")]
impl MediaSignallingRx for GstMediaSignalRx {
    fn offer(&mut self, _offer: &str) {
        // The server is the offerer; receiving an offer here is unexpected.
        warn!("V4l2Device: unexpected offer received from client (server is the offerer)");
    }

    fn answer(&mut self, answer: &str) {
        let sdp =
            gstreamer_sdp::SDPMessage::parse_buffer(answer.as_bytes()).expect("parse answer SDP");
        let desc = WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp);
        self.webrtcbin.emit_by_name::<()>(
            "set-remote-description",
            &[&desc, &None::<gstreamer::Promise>],
        );
    }

    fn ice(&mut self, candidate: &str, mline_index: u32) {
        self.webrtcbin
            .emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
    }
}

// ---------------------------------------------------------------------------
// ViewerStream — implements Stream<Item = MediaSignalMsg> and cleans up on Drop
// ---------------------------------------------------------------------------

#[cfg(feature = "gstreamer")]
struct ViewerStream {
    id: ViewerId,
    device_inner: Arc<Mutex<Option<V4l2DeviceInner>>>,
    rx: mpsc::UnboundedReceiver<MediaSignalMsg>,
}

#[cfg(feature = "gstreamer")]
impl Drop for ViewerStream {
    fn drop(&mut self) {
        let pipeline_to_teardown = {
            let mut lock = self.device_inner.lock().unwrap();
            if let Some(ref mut inner) = *lock {
                inner.remove_viewer(self.id);
                if inner.viewers.is_empty() {
                    // Take pipeline out before releasing the lock to avoid keeping the mutex
                    // locked during the (potentially blocking) state change.
                    let pipeline = inner.pipeline.clone();
                    *lock = None;
                    Some(pipeline)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(pipeline) = pipeline_to_teardown {
            info!("V4L2: last viewer disconnected, tearing down pipeline");
            let _ = pipeline.set_state(gstreamer::State::Null);
        }
    }
}

#[cfg(feature = "gstreamer")]
impl futures::Stream for ViewerStream {
    type Item = MediaSignalMsg;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// V4l2Device
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct V4l2Device {
    device_path: String,
    #[cfg(feature = "gstreamer")]
    inner: Arc<Mutex<Option<V4l2DeviceInner>>>,
}

impl V4l2Device {
    pub fn new(device_path: String) -> Self {
        Self {
            device_path,
            #[cfg(feature = "gstreamer")]
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

#[cfg(feature = "gstreamer")]
#[async_trait::async_trait]
impl Media for V4l2Device {
    async fn open(
        &self,
    ) -> Result<
        (
            Box<dyn MediaSignallingRx>,
            BoxStream<'static, MediaSignalMsg>,
        ),
        MediaError,
    > {
        gstreamer::init().map_err(|e| MediaError::Internal(e.to_string()))?;

        let (tx, rx) = mpsc::unbounded_channel::<MediaSignalMsg>();

        let (webrtcbin, viewer_id) = {
            let mut lock = self.inner.lock().unwrap();
            let inner = lock.get_or_insert_with(|| V4l2DeviceInner::build(&self.device_path));
            let id = inner.add_viewer(tx);
            let webrtcbin = inner.viewers[&id].webrtcbin.clone();
            (webrtcbin, id)
        };

        let signal_rx = Box::new(GstMediaSignalRx { webrtcbin });
        let viewer_stream = ViewerStream {
            id: viewer_id,
            device_inner: self.inner.clone(),
            rx,
        };

        Ok((signal_rx, Box::pin(viewer_stream)))
    }
}

#[cfg(not(feature = "gstreamer"))]
#[async_trait::async_trait]
impl Media for V4l2Device {
    async fn open(
        &self,
    ) -> Result<
        (
            Box<dyn MediaSignallingRx>,
            futures::stream::BoxStream<'static, MediaSignalMsg>,
        ),
        MediaError,
    > {
        Err(MediaError::NotSupported)
    }
}
