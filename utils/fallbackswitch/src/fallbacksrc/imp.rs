// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use parking_lot::Mutex;
use std::time::Instant;
use std::{cmp, mem};

use once_cell::sync::Lazy;

use super::custom_source::CustomSource;
use super::{RetryReason, Status};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc",
        gst::DebugColorFlags::empty(),
        Some("Fallback Source Bin"),
    )
});

#[derive(Debug, Clone)]
struct Stats {
    num_retry: u64,
    num_fallback_retry: u64,
    last_retry_reason: RetryReason,
    last_fallback_retry_reason: RetryReason,
    buffering_percent: i32,
    fallback_buffering_percent: i32,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            num_retry: 0,
            num_fallback_retry: 0,
            last_retry_reason: RetryReason::None,
            last_fallback_retry_reason: RetryReason::None,
            buffering_percent: 100,
            fallback_buffering_percent: 100,
        }
    }
}

impl Stats {
    fn to_structure(&self) -> gst::Structure {
        gst::Structure::builder("application/x-fallbacksrc-stats")
            .field("num-retry", self.num_retry)
            .field("num-fallback-retry", &self.num_fallback_retry)
            .field("last-retry-reason", self.last_retry_reason)
            .field(
                "last-fallback-retry-reason",
                self.last_fallback_retry_reason,
            )
            .field("buffering-percent", self.buffering_percent)
            .field(
                "fallback-buffering-percent",
                self.fallback_buffering_percent,
            )
            .build()
    }
}

#[derive(Debug, Clone)]
struct Settings {
    enable_audio: bool,
    enable_video: bool,
    uri: Option<String>,
    source: Option<gst::Element>,
    fallback_uri: Option<String>,
    timeout: gst::ClockTime,
    restart_timeout: gst::ClockTime,
    retry_timeout: gst::ClockTime,
    restart_on_eos: bool,
    min_latency: gst::ClockTime,
    buffer_duration: i64,
    immediate_fallback: bool,
    manual_unblock: bool,
    fallback_video_caps: gst::Caps,
    fallback_audio_caps: gst::Caps,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enable_audio: true,
            enable_video: true,
            uri: None,
            source: None,
            fallback_uri: None,
            timeout: 5 * gst::ClockTime::SECOND,
            restart_timeout: 5 * gst::ClockTime::SECOND,
            retry_timeout: 60 * gst::ClockTime::SECOND,
            restart_on_eos: false,
            min_latency: gst::ClockTime::ZERO,
            buffer_duration: -1,
            immediate_fallback: false,
            manual_unblock: false,
            fallback_video_caps: gst::Caps::new_any(),
            fallback_audio_caps: gst::Caps::new_any(),
        }
    }
}

#[derive(Debug)]
enum Source {
    Uri(String),
    Element(gst::Element),
}

// Blocking buffer pad probe on the source pads. Once blocked we have a running time for the
// current buffer that can later be used for offsetting
//
// This is used for the initial offsetting after starting of the stream and for "pausing" when
// buffering.
struct Block {
    pad: gst::Pad,
    probe_id: gst::PadProbeId,
    running_time: Option<gst::ClockTime>,
}

struct StreamBranch {
    // source pad from actual source inside the source bin
    source_srcpad: gst::Pad,
    // blocking pad probe on the source pad of the source queue
    source_srcpad_block: Option<Block>,

    // other elements in the source bin before the ghostpad
    // imagefreeze before the clocksync if this is a stillframe stream
    imagefreeze: Option<gst::Element>,
    clocksync: gst::Element,
    converters: gst::Element,
    queue: gst::Element,
    // queue source pad, target pad of the source ghost pad
    queue_srcpad: gst::Pad,

    // Request pad on the fallbackswitch
    switch_pad: gst::Pad,
}

// Connects one source pad with fallbackswitch and the corresponding fallback input
struct Stream {
    // Main stream and fallback stream branches to the fallback switch
    main_branch: Option<StreamBranch>,
    // If this does not exist then the fallbackswitch is connected directly to the dummy
    // audio/video sources
    fallback_branch: Option<StreamBranch>,

    // fallbackswitch
    // fallbackswitch in the main bin, linked to the ghostpads above
    switch: gst::Element,

    // output source pad on the main bin, switch source pad is ghostpad target
    srcpad: gst::GhostPad,

    // filter caps for the fallback/dummy streams
    filter_caps: gst::Caps,
}

struct SourceBin {
    // uridecodebin3 or custom source element inside a bin.
    //
    // This bin would also contain imagefreeze, clocksync and queue elements as needed for the
    // outputs and would be connected via ghost pads to the fallbackswitch elements.
    source: gst::Bin,
    pending_restart: bool,
    is_live: bool,
    is_image: bool,

    // For timing out the source and shutting it down to restart it
    restart_timeout: Option<gst::SingleShotClockId>,
    // For restarting the source after shutting it down
    pending_restart_timeout: Option<gst::SingleShotClockId>,
    // For failing completely if we didn't recover after the retry timeout
    retry_timeout: Option<gst::SingleShotClockId>,

    // Stream collection posted by source
    streams: Option<gst::StreamCollection>,
}

struct State {
    source: SourceBin,
    fallback_source: Option<SourceBin>,

    // audio/video dummy source if the fallback source fails or is not started yet
    audio_dummy_source: Option<gst::Bin>,
    video_dummy_source: Option<gst::Bin>,

    // All our output streams, selected by properties
    video_stream: Option<Stream>,
    audio_stream: Option<Stream>,
    flow_combiner: gst_base::UniqueFlowCombiner,

    last_buffering_update: Option<Instant>,
    fallback_last_buffering_update: Option<Instant>,

    // Configure settings
    settings: Settings,
    configured_source: Source,

    // Statistics
    stats: Stats,

    // When application is using the manual-unblock property
    manually_blocked: bool,
    // So that we don't schedule a restart when manually unblocking
    // and our source hasn't reached the required state
    schedule_restart_on_unblock: bool,
}

#[derive(Default)]
pub struct FallbackSrc {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for FallbackSrc {
    const NAME: &'static str = "FallbackSrc";
    type Type = super::FallbackSrc;
    type ParentType = gst::Bin;
}

impl ObjectImpl for FallbackSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("enable-audio")
                    .nick("Enable Audio")
                    .blurb("Enable the audio stream, this will output silence if there's no audio in the configured URI")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("enable-video")
                    .nick("Enable Video")
                    .blurb("Enable the video stream, this will output black or the fallback video if there's no video in the configured URI")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("uri")
                    .nick("URI")
                    .blurb("URI to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gst::Element>("source")
                    .nick("Source")
                    .blurb("Source to use instead of the URI")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("fallback-uri")
                    .nick("Fallback URI")
                    .blurb("Fallback URI to use for video in case the main stream doesn't work")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("timeout")
                    .nick("Timeout")
                    .blurb("Timeout for switching to the fallback URI")
                    .maximum(std::u64::MAX - 1)
                    .default_value(5 * *gst::ClockTime::SECOND)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("restart-timeout")
                    .nick("Timeout")
                    .blurb("Timeout for restarting an active source")
                    .maximum(std::u64::MAX - 1)
                    .default_value(5 * *gst::ClockTime::SECOND)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("retry-timeout")
                    .nick("Retry Timeout")
                    .blurb("Timeout for stopping after repeated failure")
                    .maximum(std::u64::MAX - 1)
                    .default_value(60 * *gst::ClockTime::SECOND)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("restart-on-eos")
                    .nick("Restart on EOS")
                    .blurb("Restart source on EOS")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<Status>("status", Status::Stopped)
                    .nick("Status")
                    .blurb("Current source status")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("min-latency")
                    .nick("Minimum Latency")
                    .blurb("When the main source has a higher latency than the fallback source \
                     this allows to configure a minimum latency that would be configured \
                     if initially the fallback is enabled")
                    .maximum(std::u64::MAX - 1)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecInt64::builder("buffer-duration")
                    .nick("Buffer Duration")
                    .blurb("Buffer duration when buffering streams (-1 default value)")
                    .minimum(-1)
                    .maximum(std::i64::MAX - 1)
                    .default_value(-1)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("statistics")
                    .nick("Statistics")
                    .blurb("Various statistics")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("manual-unblock")
                    .nick("Manual unblock")
                    .blurb("When enabled, the application must call the unblock signal, except for live streams")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("immediate-fallback")
                    .nick("Immediate fallback")
                    .blurb("Forward the fallback streams immediately at startup, when the primary streams are slow to start up and immediate output is required")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("fallback-video-caps")
                    .nick("Fallback Video Caps")
                    .blurb("Raw video caps for fallback stream")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("fallback-audio-caps")
                    .nick("Fallback Audio Caps")
                    .blurb("Raw audio caps for fallback stream")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "enable-audio" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing enable-audio from {:?} to {:?}",
                    settings.enable_audio,
                    new_value,
                );
                settings.enable_audio = new_value;
            }
            "enable-video" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing enable-video from {:?} to {:?}",
                    settings.enable_video,
                    new_value,
                );
                settings.enable_video = new_value;
            }
            "uri" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing URI from {:?} to {:?}",
                    settings.uri,
                    new_value,
                );
                settings.uri = new_value;
            }
            "source" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing source from {:?} to {:?}",
                    settings.source,
                    new_value,
                );
                settings.source = new_value;
            }
            "fallback-uri" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing Fallback URI from {:?} to {:?}",
                    settings.fallback_uri,
                    new_value,
                );
                settings.fallback_uri = new_value;
            }
            "timeout" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing timeout from {:?} to {:?}",
                    settings.timeout,
                    new_value,
                );
                settings.timeout = new_value;
            }
            "restart-timeout" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing Restart Timeout from {:?} to {:?}",
                    settings.restart_timeout,
                    new_value,
                );
                settings.restart_timeout = new_value;
            }
            "retry-timeout" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing Retry Timeout from {:?} to {:?}",
                    settings.retry_timeout,
                    new_value,
                );
                settings.retry_timeout = new_value;
            }
            "restart-on-eos" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing restart-on-eos from {:?} to {:?}",
                    settings.restart_on_eos,
                    new_value,
                );
                settings.restart_on_eos = new_value;
            }
            "min-latency" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing Minimum Latency from {:?} to {:?}",
                    settings.min_latency,
                    new_value,
                );
                settings.min_latency = new_value;
            }
            "buffer-duration" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing Buffer Duration from {:?} to {:?}",
                    settings.buffer_duration,
                    new_value,
                );
                settings.buffer_duration = new_value;
            }
            "immediate-fallback" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing immediate-fallback from {:?} to {:?}",
                    settings.immediate_fallback,
                    new_value,
                );
                settings.immediate_fallback = new_value;
            }
            "manual-unblock" => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing manual-unblock from {:?} to {:?}",
                    settings.manual_unblock,
                    new_value,
                );
                settings.manual_unblock = new_value;
            }
            "fallback-video-caps" => {
                let mut settings = self.settings.lock();
                let new_value = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_any);
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing fallback video caps from {} to {}",
                    settings.fallback_video_caps,
                    new_value,
                );
                settings.fallback_video_caps = new_value;
            }
            "fallback-audio-caps" => {
                let mut settings = self.settings.lock();
                let new_value = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_any);
                gst::info!(
                    CAT,
                    obj: obj,
                    "Changing fallback audio caps from {} to {}",
                    settings.fallback_audio_caps,
                    new_value,
                );
                settings.fallback_audio_caps = new_value;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    #[allow(clippy::blocks_in_if_conditions)]
    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "enable-audio" => {
                let settings = self.settings.lock();
                settings.enable_audio.to_value()
            }
            "enable-video" => {
                let settings = self.settings.lock();
                settings.enable_video.to_value()
            }
            "uri" => {
                let settings = self.settings.lock();
                settings.uri.to_value()
            }
            "source" => {
                let settings = self.settings.lock();
                settings.source.to_value()
            }
            "fallback-uri" => {
                let settings = self.settings.lock();
                settings.fallback_uri.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock();
                settings.timeout.to_value()
            }
            "restart-timeout" => {
                let settings = self.settings.lock();
                settings.restart_timeout.to_value()
            }
            "retry-timeout" => {
                let settings = self.settings.lock();
                settings.retry_timeout.to_value()
            }
            "restart-on-eos" => {
                let settings = self.settings.lock();
                settings.restart_on_eos.to_value()
            }
            "status" => {
                let state_guard = self.state.lock();

                // If we have no state then we'r stopped
                let state = match &*state_guard {
                    None => return Status::Stopped.to_value(),
                    Some(ref state) => state,
                };

                // If any restarts/retries are pending, we're retrying
                if state.source.pending_restart
                    || state.source.pending_restart_timeout.is_some()
                    || state.source.retry_timeout.is_some()
                {
                    return Status::Retrying.to_value();
                }

                // Otherwise if buffering < 100, we have no streams yet or of the expected
                // streams there is no source pad yet, we're buffering
                let mut have_audio = false;
                let mut have_video = false;
                if let Some(ref streams) = state.source.streams {
                    for stream in streams.iter() {
                        have_audio =
                            have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
                        have_video =
                            have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
                    }
                }

                if state.stats.buffering_percent < 100
                    || state.source.restart_timeout.is_some()
                    || state.source.streams.is_none()
                    || (have_audio
                        && state
                            .audio_stream
                            .as_ref()
                            .and_then(|s| s.main_branch.as_ref())
                            .map(|b| b.source_srcpad_block.is_some())
                            .unwrap_or(true))
                    || (have_video
                        && state
                            .video_stream
                            .as_ref()
                            .and_then(|s| s.main_branch.as_ref())
                            .map(|b| b.source_srcpad_block.is_some())
                            .unwrap_or(true))
                {
                    return Status::Buffering.to_value();
                }

                // Otherwise we're running now
                Status::Running.to_value()
            }
            "min-latency" => {
                let settings = self.settings.lock();
                settings.min_latency.to_value()
            }
            "buffer-duration" => {
                let settings = self.settings.lock();
                settings.buffer_duration.to_value()
            }
            "statistics" => self.stats().to_value(),
            "immediate-fallback" => {
                let settings = self.settings.lock();
                settings.immediate_fallback.to_value()
            }
            "manual-unblock" => {
                let settings = self.settings.lock();
                settings.manual_unblock.to_value()
            }
            "fallback-video-caps" => {
                let settings = self.settings.lock();
                settings.fallback_video_caps.to_value()
            }
            "fallback-audio-caps" => {
                let settings = self.settings.lock();
                settings.fallback_audio_caps.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("update-uri")
                    .param_types([String::static_type()])
                    .return_type::<String>()
                    .class_handler(|_token, args| {
                        // Simply return the input by default
                        Some(args[1].clone())
                    })
                    .accumulator(|_hint, ret, value| {
                        // First signal handler wins
                        *ret = value.clone();
                        false
                    })
                    .build(),
                glib::subclass::Signal::builder("unblock")
                    .action()
                    .class_handler(|_token, args| {
                        let element = args[0].get::<super::FallbackSrc>().expect("signal arg");
                        let src = element.imp();
                        let mut state_guard = src.state.lock();
                        let state = match &mut *state_guard {
                            None => {
                                return None;
                            }
                            Some(state) => state,
                        };

                        state.manually_blocked = false;

                        if state.schedule_restart_on_unblock
                            && src.have_fallback_activated(&element, state)
                        {
                            src.schedule_source_restart_timeout(
                                &element,
                                state,
                                gst::ClockTime::ZERO,
                                false,
                            );
                        }

                        src.unblock_pads(&element, state, false);

                        None
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl GstObjectImpl for FallbackSrc {}

impl ElementImpl for FallbackSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            #[cfg(feature = "doc")]
            Status::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
            gst::subclass::ElementMetadata::new(
                "Fallback Source",
                "Generic/Source",
                "Live source with uridecodebin3 or custom source, and fallback stream",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let audio_src_pad_template = gst::PadTemplate::new(
                "audio",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let video_src_pad_template = gst::PadTemplate::new(
                "video",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![audio_src_pad_template, video_src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.start(element)?;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
            .map_err(|err| {
                gst::error!(
                    CAT,
                    obj: element,
                    "Parent state change transition {:?} failed",
                    transition
                );

                err
            })?;

        // Change the source state manually here to be able to catch errors. State changes always
        // happen from sink to source, so we do this after chaining up.
        self.change_source_state(element, transition, false);

        // Change the fallback source state manually here to be able to catch errors. State changes always
        // happen from sink to source, so we do this after chaining up.
        self.change_source_state(element, transition, true);

        // Ignore parent state change return to prevent spurious async/no-preroll return values
        // due to core state change bugs
        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                Ok(gst::StateChangeSuccess::NoPreroll)
            }
            gst::StateChange::ReadyToNull => {
                self.stop(element);
                Ok(gst::StateChangeSuccess::Success)
            }
            _ => Ok(gst::StateChangeSuccess::Success),
        }
    }

    fn send_event(&self, element: &Self::Type, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(..) => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Handling element-level EOS, forwarding to all streams"
                );

                let mut state_guard = self.state.lock();
                let state = match &mut *state_guard {
                    None => {
                        return true;
                    }
                    Some(state) => state,
                };

                // We don't want to hold the state lock while pushing out EOS
                let mut send_eos_elements = vec![];
                let mut send_eos_pads = vec![];

                send_eos_elements.push(state.source.source.clone());

                // Not strictly necessary as the switch will EOS when receiving
                // EOS on its primary pad, just good form.
                if let Some(ref source) = state.fallback_source {
                    send_eos_elements.push(source.source.clone());
                }
                if let Some(ref source) = state.audio_dummy_source {
                    send_eos_elements.push(source.clone());
                }
                if let Some(ref source) = state.video_dummy_source {
                    send_eos_elements.push(source.clone());
                }

                for branch in [&mut state.video_stream, &mut state.audio_stream]
                    .iter_mut()
                    .filter_map(|v| v.as_mut())
                    .flat_map(|s| [s.main_branch.as_mut(), s.fallback_branch.as_mut()])
                    .flatten()
                {
                    // If our source hadn't been connected to the switch as a primary
                    // stream, we need to send EOS there ourselves
                    let queue_sinkpad = branch.queue.static_pad("sink").unwrap();
                    send_eos_pads.push(queue_sinkpad.clone());
                }

                drop(state_guard);

                for elem in send_eos_elements {
                    elem.send_event(event.clone());
                }

                for pad in send_eos_pads {
                    pad.send_event(event.clone());
                }

                true
            }
            _ => true,
        }
    }
}

impl BinImpl for FallbackSrc {
    fn handle_message(&self, bin: &Self::Type, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Buffering(m) => {
                // Don't forward upwards, we handle this internally
                self.handle_buffering(bin, m);
            }
            MessageView::StreamsSelected(m) => {
                // Don't forward upwards, we are exposing streams based on properties
                // TODO: Do stream configuration via our own stream collection and handling
                // of stream select events
                // TODO: Also needs updating of StreamCollection handling in CustomSource
                self.handle_streams_selected(bin, m);
            }
            MessageView::Error(m) => {
                if !self.handle_error(bin, m) {
                    self.parent_handle_message(bin, msg);
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}

impl FallbackSrc {
    fn create_dummy_audio_source(filter_caps: &gst::Caps, min_latency: gst::ClockTime) -> gst::Bin {
        let bin = gst::Bin::new(None);

        let audiotestsrc = gst::ElementFactory::make("audiotestsrc", Some("audiosrc"))
            .expect("No audiotestsrc found");

        let audioconvert = gst::ElementFactory::make("audioconvert", Some("audio_audioconvert"))
            .expect("No audioconvert found");

        let audioresample = gst::ElementFactory::make("audioresample", Some("audio_audioresample"))
            .expect("No audioresample found");

        let capsfilter = gst::ElementFactory::make("capsfilter", Some("audio_capsfilter"))
            .expect("No capsfilter found");

        let queue = gst::ElementFactory::make("queue", None).expect("No queue found");

        audiotestsrc.set_property_from_str("wave", "silence");
        audiotestsrc.set_property("is-live", true);

        capsfilter.set_property("caps", filter_caps);

        queue.set_properties(&[
            ("max-size-bytes", &0u32),
            ("max-size-buffers", &0u32),
            (
                "max-size-time",
                &(cmp::max(min_latency, gst::ClockTime::from_seconds(1))),
            ),
        ]);

        bin.add_many(&[
            &audiotestsrc,
            &audioconvert,
            &audioresample,
            &capsfilter,
            &queue,
        ])
        .unwrap();

        gst::Element::link_many(&[
            &audiotestsrc,
            &audioconvert,
            &audioresample,
            &capsfilter,
            &queue,
        ])
        .unwrap();

        let ghostpad =
            gst::GhostPad::with_target(Some("src"), &queue.static_pad("src").unwrap()).unwrap();
        ghostpad.set_active(true).unwrap();
        bin.add_pad(&ghostpad).unwrap();

        bin
    }

    fn create_dummy_video_source(filter_caps: &gst::Caps, min_latency: gst::ClockTime) -> gst::Bin {
        let bin = gst::Bin::new(None);

        let videotestsrc = gst::ElementFactory::make("videotestsrc", Some("videosrc"))
            .expect("No videotestsrc found");

        let videoconvert = gst::ElementFactory::make("videoconvert", Some("video_videoconvert"))
            .expect("No videoconvert found");

        let videoscale = gst::ElementFactory::make("videoscale", Some("video_videoscale"))
            .expect("No videoscale found");

        let capsfilter = gst::ElementFactory::make("capsfilter", Some("video_capsfilter"))
            .expect("No capsfilter found");

        let queue = gst::ElementFactory::make("queue", None).expect("No queue found");

        videotestsrc.set_property_from_str("pattern", "black");
        videotestsrc.set_property("is-live", true);

        capsfilter.set_property("caps", filter_caps);

        queue.set_properties(&[
            ("max-size-bytes", &0u32),
            ("max-size-buffers", &0u32),
            (
                "max-size-time",
                &(cmp::max(min_latency, gst::ClockTime::from_seconds(1))),
            ),
        ]);

        bin.add_many(&[
            &videotestsrc,
            &videoconvert,
            &videoscale,
            &capsfilter,
            &queue,
        ])
        .unwrap();

        gst::Element::link_many(&[
            &videotestsrc,
            &videoconvert,
            &videoscale,
            &capsfilter,
            &queue,
        ])
        .unwrap();

        let ghostpad =
            gst::GhostPad::with_target(Some("src"), &queue.static_pad("src").unwrap()).unwrap();
        ghostpad.set_active(true).unwrap();
        bin.add_pad(&ghostpad).unwrap();

        bin
    }

    fn create_main_input(
        &self,
        element: &super::FallbackSrc,
        source: &Source,
        buffer_duration: i64,
    ) -> SourceBin {
        let bin = gst::Bin::new(None);

        let source = match source {
            Source::Uri(ref uri) => {
                let source = gst::ElementFactory::make("uridecodebin3", Some("uridecodebin"))
                    .expect("No uridecodebin3 found");

                let uri = element.emit_by_name::<glib::GString>("update-uri", &[uri]);

                source.set_property("uri", uri);
                source.set_property("use-buffering", true);
                source.set_property("buffer-duration", buffer_duration);

                source
            }
            Source::Element(ref source) => CustomSource::new(source).upcast(),
        };

        bin.add(&source).unwrap();

        // Handle any async state changes internally, they don't affect the pipeline because we
        // convert everything to a live stream
        bin.set_property("async-handling", true);
        // Don't let the bin handle state changes of the source. We want to do it manually to catch
        // possible errors and retry, without causing the whole bin state change to fail
        bin.set_locked_state(true);

        let element_weak = element.downgrade();
        source.connect_pad_added(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = element.imp();

            if let Err(msg) = src.handle_source_pad_added(&element, pad, false) {
                element.post_error_message(msg);
            }
        });
        let element_weak = element.downgrade();
        source.connect_pad_removed(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = element.imp();

            src.handle_source_pad_removed(&element, pad, false);
        });

        element.add(&bin).unwrap();

        SourceBin {
            source: bin,
            pending_restart: false,
            is_live: false,
            is_image: false,
            restart_timeout: None,
            pending_restart_timeout: None,
            retry_timeout: None,
            streams: None,
        }
    }

    fn create_fallback_input(
        &self,
        element: &super::FallbackSrc,
        fallback_uri: Option<&str>,
        buffer_duration: i64,
    ) -> Option<SourceBin> {
        let source: gst::Element = match fallback_uri {
            Some(uri) => {
                let dbin = gst::ElementFactory::make("uridecodebin3", Some("uridecodebin"))
                    .expect("No uridecodebin3 found");
                dbin.set_property("uri", uri);
                dbin.set_property("use-buffering", true);
                dbin.set_property("buffer-duration", buffer_duration);

                dbin
            }
            None => return None,
        };

        let bin = gst::Bin::new(None);

        bin.add(&source).unwrap();

        let element_weak = element.downgrade();
        source.connect_pad_added(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = FallbackSrc::from_instance(&element);

            if let Err(msg) = src.handle_source_pad_added(&element, pad, true) {
                element.post_error_message(msg);
            }
        });
        let element_weak = element.downgrade();
        source.connect_pad_removed(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = FallbackSrc::from_instance(&element);

            src.handle_source_pad_removed(&element, pad, true);
        });

        // Handle any async state changes internally, they don't affect the pipeline because we
        // convert everything to a live stream
        bin.set_property("async-handling", true);
        // Don't let the bin handle state changes of the dbin. We want to do it manually to catch
        // possible errors and retry, without causing the whole bin state change to fail
        bin.set_locked_state(true);

        element.add(&bin).unwrap();

        Some(SourceBin {
            source: bin,
            pending_restart: false,
            is_live: false,
            is_image: false,
            restart_timeout: None,
            pending_restart_timeout: None,
            retry_timeout: None,
            streams: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_stream(
        &self,
        element: &super::FallbackSrc,
        timeout: gst::ClockTime,
        min_latency: gst::ClockTime,
        is_audio: bool,
        immediate_fallback: bool,
        dummy_source: &gst::Bin,
        filter_caps: &gst::Caps,
    ) -> Stream {
        let switch =
            gst::ElementFactory::make("fallbackswitch", None).expect("No fallbackswitch found");

        element.add(&switch).unwrap();

        switch.set_property("timeout", timeout.nseconds());
        switch.set_property("min-upstream-latency", min_latency.nseconds());
        switch.set_property("immediate-fallback", immediate_fallback);

        let dummy_srcpad = dummy_source.static_pad("src").unwrap();
        let dummy_sinkpad = switch.request_pad_simple("sink_%u").unwrap();
        dummy_srcpad.link(&dummy_sinkpad).unwrap();
        dummy_sinkpad.set_property("priority", 2u32);

        let element_weak = element.downgrade();
        switch.connect_notify(Some("active-pad"), move |_switch, _pspec| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };

            let src = element.imp();
            src.handle_switch_active_pad_change(&element, is_audio);
        });

        let srcpad = switch.static_pad("src").unwrap();
        let templ = element
            .pad_template(if is_audio { "audio" } else { "video" })
            .unwrap();
        let ghostpad = gst::GhostPad::builder_with_template(&templ, Some(&templ.name()))
            .proxy_pad_chain_function({
                let element_weak = element.downgrade();
                move |pad, _parent, buffer| {
                    let element = match element_weak.upgrade() {
                        None => return Err(gst::FlowError::Flushing),
                        Some(element) => element,
                    };

                    let src = element.imp();
                    src.proxy_pad_chain(&element, pad, buffer)
                }
            })
            .build_with_target(&srcpad)
            .unwrap();

        let _ = ghostpad.set_active(true);

        element.add_pad(&ghostpad).unwrap();

        Stream {
            main_branch: None,
            fallback_branch: None,
            switch,
            srcpad: ghostpad.upcast(),
            filter_caps: filter_caps.clone(),
        }
    }

    fn start(&self, element: &super::FallbackSrc) -> Result<(), gst::StateChangeError> {
        gst::debug!(CAT, obj: element, "Starting");
        let mut state_guard = self.state.lock();
        if state_guard.is_some() {
            return Err(gst::StateChangeError);
        }

        let settings = self.settings.lock().clone();
        let configured_source = match settings
            .uri
            .as_ref()
            .cloned()
            .map(Source::Uri)
            .or_else(|| settings.source.as_ref().cloned().map(Source::Element))
        {
            Some(source) => source,
            None => {
                gst::error!(CAT, obj: element, "No URI or source element configured");
                gst::element_error!(
                    element,
                    gst::LibraryError::Settings,
                    ["No URI or source element configured"]
                );
                return Err(gst::StateChangeError);
            }
        };

        let fallback_uri = &settings.fallback_uri;

        // Create main input
        let source = self.create_main_input(element, &configured_source, settings.buffer_duration);

        // Create fallback input
        let fallback_source =
            self.create_fallback_input(element, fallback_uri.as_deref(), settings.buffer_duration);

        let mut flow_combiner = gst_base::UniqueFlowCombiner::new();

        // Create video stream and video dummy input
        let (video_stream, video_dummy_source) = if settings.enable_video {
            let video_dummy_source = Self::create_dummy_video_source(
                &settings.fallback_video_caps,
                settings.min_latency,
            );
            element.add(&video_dummy_source).unwrap();

            let stream = self.create_stream(
                element,
                settings.timeout,
                settings.min_latency,
                false,
                settings.immediate_fallback,
                &video_dummy_source,
                &settings.fallback_video_caps,
            );
            flow_combiner.add_pad(&stream.srcpad);

            (Some(stream), Some(video_dummy_source))
        } else {
            (None, None)
        };

        // Create audio stream and out dummy input
        let (audio_stream, audio_dummy_source) = if settings.enable_audio {
            let audio_dummy_source = Self::create_dummy_audio_source(
                &settings.fallback_audio_caps,
                settings.min_latency,
            );
            element.add(&audio_dummy_source).unwrap();

            let stream = self.create_stream(
                element,
                settings.timeout,
                settings.min_latency,
                true,
                settings.immediate_fallback,
                &audio_dummy_source,
                &settings.fallback_audio_caps,
            );
            flow_combiner.add_pad(&stream.srcpad);

            (Some(stream), Some(audio_dummy_source))
        } else {
            (None, None)
        };

        let manually_blocked = settings.manual_unblock;

        *state_guard = Some(State {
            source,
            fallback_source,
            video_stream,
            audio_stream,
            audio_dummy_source,
            video_dummy_source,
            flow_combiner,
            last_buffering_update: None,
            fallback_last_buffering_update: None,
            settings,
            configured_source,
            stats: Stats::default(),
            manually_blocked,
            schedule_restart_on_unblock: false,
        });

        drop(state_guard);

        element.no_more_pads();

        element.notify("status");

        gst::debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn stop(&self, element: &super::FallbackSrc) {
        gst::debug!(CAT, obj: element, "Stopping");
        let mut state_guard = self.state.lock();
        let mut state = match state_guard.take() {
            Some(state) => state,
            None => return,
        };
        drop(state_guard);

        element.notify("status");

        // In theory all streams should've been removed from the source's pad-removed signal
        // handler when going from Paused to Ready but better safe than sorry here
        for stream in [&state.video_stream, &state.audio_stream]
            .iter()
            .filter_map(|v| v.as_ref())
        {
            for branch in [&stream.main_branch, &stream.fallback_branch]
                .iter()
                .filter_map(|v| v.as_ref())
            {
                element.remove(&branch.queue).unwrap();
                element.remove(&branch.converters).unwrap();
                element.remove(&branch.clocksync).unwrap();
                if let Some(ref imagefreeze) = branch.imagefreeze {
                    element.remove(imagefreeze).unwrap();
                }
                if branch.switch_pad.parent().as_ref() == Some(stream.switch.upcast_ref()) {
                    stream.switch.release_request_pad(&branch.switch_pad);
                }
            }
            element.remove(&stream.switch).unwrap();
            let _ = stream.srcpad.set_target(None::<&gst::Pad>);
            let _ = element.remove_pad(&stream.srcpad);
        }
        state.video_stream = None;
        state.audio_stream = None;

        if let Source::Element(ref source) = state.configured_source {
            // Explicitly remove the source element from the CustomSource so that we can
            // later create a new CustomSource and add it again there.
            if source.has_as_parent(&state.source.source) {
                let _ = source.set_state(gst::State::Null);
                let _ = state
                    .source
                    .source
                    .downcast_ref::<gst::Bin>()
                    .unwrap()
                    .remove(source);
            }
        }

        for source in [Some(&mut state.source), state.fallback_source.as_mut()]
            .iter_mut()
            .flatten()
        {
            element.remove(&source.source).unwrap();

            if let Some(timeout) = source.pending_restart_timeout.take() {
                timeout.unschedule();
            }

            if let Some(timeout) = source.retry_timeout.take() {
                timeout.unschedule();
            }

            if let Some(timeout) = source.restart_timeout.take() {
                timeout.unschedule();
            }
        }

        for source in [
            state.video_dummy_source.take(),
            state.audio_dummy_source.take(),
        ]
        .iter()
        .flatten()
        {
            let _ = source.set_state(gst::State::Null);
            element.remove(source).unwrap();
        }

        gst::debug!(CAT, obj: element, "Stopped");
    }

    fn change_source_state(
        &self,
        element: &super::FallbackSrc,
        transition: gst::StateChange,
        fallback_source: bool,
    ) {
        gst::debug!(
            CAT,
            obj: element,
            "Changing {}source state: {:?}",
            if fallback_source { "fallback " } else { "" },
            transition
        );
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            Some(state) => state,
            None => return,
        };

        let mut source = if fallback_source {
            if let Some(ref mut source) = state.fallback_source {
                source
            } else {
                return;
            }
        } else {
            &mut state.source
        };

        if transition.current() <= transition.next() && source.pending_restart {
            gst::debug!(
                CAT,
                obj: element,
                "Not starting {}source because pending restart",
                if fallback_source { "fallback " } else { "" }
            );
            return;
        } else if transition.next() <= gst::State::Ready && source.pending_restart {
            gst::debug!(
                CAT,
                obj: element,
                "Unsetting pending {}restart because shutting down",
                if fallback_source { "fallback " } else { "" }
            );
            source.pending_restart = false;
            if let Some(timeout) = source.pending_restart_timeout.take() {
                timeout.unschedule();
            }
        }
        let source = source.source.clone();
        drop(state_guard);

        element.notify("status");

        let res = source.set_state(transition.next());
        match res {
            Err(_) => {
                gst::error!(
                    CAT,
                    obj: element,
                    "{}source failed to change state",
                    if fallback_source { "fallback " } else { "" }
                );
                // Try again later if we're not shutting down
                if transition != gst::StateChange::ReadyToNull {
                    let _ = source.set_state(gst::State::Null);
                    let mut state_guard = self.state.lock();
                    let state = state_guard.as_mut().expect("no state");
                    self.handle_source_error(
                        element,
                        state,
                        RetryReason::StateChangeFailure,
                        fallback_source,
                    );
                    drop(state_guard);
                    element.notify("statistics");
                }
            }
            Ok(res) => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "{}source changed state successfully: {:?}",
                    if fallback_source { "fallback " } else { "" },
                    res
                );

                let mut state_guard = self.state.lock();
                let state = state_guard.as_mut().expect("no state");

                let source = if fallback_source {
                    if let Some(ref mut source) = state.fallback_source {
                        source
                    } else {
                        return;
                    }
                } else {
                    &mut state.source
                };

                // Remember if the source is live
                if transition == gst::StateChange::ReadyToPaused {
                    source.is_live = res == gst::StateChangeSuccess::NoPreroll;
                }

                if (!source.is_live && transition == gst::StateChange::ReadyToPaused)
                    || (source.is_live && transition == gst::StateChange::PausedToPlaying)
                {
                    if !fallback_source {
                        state.schedule_restart_on_unblock = true;
                    }
                    if source.restart_timeout.is_none() {
                        self.schedule_source_restart_timeout(
                            element,
                            state,
                            gst::ClockTime::ZERO,
                            fallback_source,
                        );
                    }
                } else if (!source.is_live && transition == gst::StateChange::PausedToReady)
                    || (source.is_live && transition == gst::StateChange::PlayingToPaused)
                {
                    if let Some(timeout) = source.pending_restart_timeout.take() {
                        timeout.unschedule();
                    }

                    if let Some(timeout) = source.retry_timeout.take() {
                        timeout.unschedule();
                    }

                    if let Some(timeout) = source.restart_timeout.take() {
                        timeout.unschedule();
                    }
                }
            }
        }
    }

    fn proxy_pad_chain(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::ProxyPad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let res = gst::ProxyPad::chain_default(pad, Some(element), buffer);

        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => return res,
            Some(state) => state,
        };

        state.flow_combiner.update_pad_flow(pad, res)
    }

    fn handle_source_pad_added(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
        fallback_source: bool,
    ) -> Result<(), gst::ErrorMessage> {
        gst::debug!(
            CAT,
            obj: element,
            "Pad {} added to {}source",
            pad.name(),
            if fallback_source { "fallback " } else { "" }
        );

        let mut is_image = false;

        if let Some(ev) = pad.sticky_event::<gst::event::StreamStart>(0) {
            let stream = ev.stream();

            if let Some(caps) = stream.and_then(|s| s.caps()) {
                if let Some(s) = caps.structure(0) {
                    is_image = s.name().starts_with("image/");
                }
            }
        }

        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        let source = if fallback_source {
            if let Some(ref mut source) = state.fallback_source {
                source
            } else {
                return Ok(());
            }
        } else {
            &mut state.source
        };

        if is_image {
            if let Some(timeout) = source.pending_restart_timeout.take() {
                timeout.unschedule();
            }

            if let Some(timeout) = source.retry_timeout.take() {
                timeout.unschedule();
            }

            if let Some(timeout) = source.restart_timeout.take() {
                timeout.unschedule();
            }
        }

        source.is_image |= is_image;

        let (is_video, stream) = match pad.name() {
            x if x.starts_with("audio") => (false, &mut state.audio_stream),
            x if x.starts_with("video") => (true, &mut state.video_stream),
            _ => {
                let caps = match pad.current_caps().unwrap_or_else(|| pad.query_caps(None)) {
                    caps if !caps.is_any() && !caps.is_empty() => caps,
                    _ => return Ok(()),
                };

                let s = caps.structure(0).unwrap();

                if s.name().starts_with("audio/") {
                    (false, &mut state.audio_stream)
                } else if s.name().starts_with("video/") {
                    (true, &mut state.video_stream)
                } else {
                    // TODO: handle subtitles etc
                    return Ok(());
                }
            }
        };

        let type_ = if is_video { "video" } else { "audio" };

        let (branch_storage, filter_caps, switch) = match stream {
            None => {
                gst::debug!(CAT, obj: element, "No {} stream enabled", type_);
                return Ok(());
            }
            Some(Stream {
                ref mut main_branch,
                ref switch,
                ref filter_caps,
                ..
            }) if !fallback_source => {
                if main_branch.is_some() {
                    gst::debug!(CAT, obj: element, "Already configured a {} stream", type_);
                    return Ok(());
                }

                (main_branch, filter_caps, switch)
            }
            Some(Stream {
                ref mut fallback_branch,
                ref switch,
                ref filter_caps,
                ..
            }) => {
                if fallback_branch.is_some() {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Already configured a {} fallback stream",
                        type_
                    );
                    return Ok(());
                }

                (fallback_branch, filter_caps, switch)
            }
        };

        let converters = if is_video {
            let bin = gst::Bin::new(None);

            let videoconvert =
                gst::ElementFactory::make("videoconvert", Some("video_videoconvert"))
                    .expect("No videoconvert found");

            let videoscale = gst::ElementFactory::make("videoscale", Some("video_videoscale"))
                .expect("No videoscale found");

            let capsfilter = gst::ElementFactory::make("capsfilter", Some("video_capsfilter"))
                .expect("No capsfilter found");

            capsfilter.set_property("caps", filter_caps);

            bin.add_many(&[&videoconvert, &videoscale, &capsfilter])
                .unwrap();

            gst::Element::link_many(&[&videoconvert, &videoscale, &capsfilter]).unwrap();

            let ghostpad =
                gst::GhostPad::with_target(Some("sink"), &videoconvert.static_pad("sink").unwrap())
                    .unwrap();
            ghostpad.set_active(true).unwrap();
            bin.add_pad(&ghostpad).unwrap();

            let ghostpad =
                gst::GhostPad::with_target(Some("src"), &capsfilter.static_pad("src").unwrap())
                    .unwrap();
            ghostpad.set_active(true).unwrap();
            bin.add_pad(&ghostpad).unwrap();

            bin.upcast()
        } else {
            let bin = gst::Bin::new(None);

            let audioconvert =
                gst::ElementFactory::make("audioconvert", Some("audio_audioconvert"))
                    .expect("No audioconvert found");

            let audioresample =
                gst::ElementFactory::make("audioresample", Some("audio_audioresample"))
                    .expect("No audioresample found");

            let capsfilter = gst::ElementFactory::make("capsfilter", Some("audio_capsfilter"))
                .expect("No capsfilter found");

            capsfilter.set_property("caps", filter_caps);

            bin.add_many(&[&audioconvert, &audioresample, &capsfilter])
                .unwrap();

            gst::Element::link_many(&[&audioconvert, &audioresample, &capsfilter]).unwrap();

            let ghostpad =
                gst::GhostPad::with_target(Some("sink"), &audioconvert.static_pad("sink").unwrap())
                    .unwrap();
            ghostpad.set_active(true).unwrap();
            bin.add_pad(&ghostpad).unwrap();

            let ghostpad =
                gst::GhostPad::with_target(Some("src"), &capsfilter.static_pad("src").unwrap())
                    .unwrap();
            ghostpad.set_active(true).unwrap();
            bin.add_pad(&ghostpad).unwrap();

            bin.upcast()
        };

        let queue = gst::ElementFactory::make("queue", None).unwrap();
        queue.set_properties(&[
            ("max-size-bytes", &0u32),
            ("max-size-buffers", &0u32),
            (
                "max-size-time",
                &(cmp::max(state.settings.min_latency, gst::ClockTime::from_seconds(1))),
            ),
        ]);
        let clocksync = gst::ElementFactory::make("clocksync", None).unwrap_or_else(|_| {
            let identity = gst::ElementFactory::make("identity", None).unwrap();
            identity.set_property("sync", true);
            identity
        });

        source
            .source
            .add_many(&[&converters, &queue, &clocksync])
            .unwrap();
        converters.sync_state_with_parent().unwrap();
        queue.sync_state_with_parent().unwrap();
        clocksync.sync_state_with_parent().unwrap();

        let sinkpad = converters.static_pad("sink").unwrap();
        pad.link(&sinkpad).map_err(|err| {
            gst::error!(CAT, obj: element, "Failed to link new source pad: {}", err);
            gst::error_msg!(
                gst::CoreError::Negotiation,
                ["Failed to link new source pad: {}", err]
            )
        })?;

        let imagefreeze = if is_image {
            gst::debug!(CAT, obj: element, "Image stream, inserting imagefreeze");
            let imagefreeze =
                gst::ElementFactory::make("imagefreeze", None).expect("no imagefreeze found");
            source.source.add(&imagefreeze).unwrap();
            imagefreeze.set_property("is-live", true);

            if imagefreeze.sync_state_with_parent().is_err() {
                gst::error!(CAT, obj: element, "imagefreeze failed to change state",);
                return Err(gst::error_msg!(
                    gst::CoreError::StateChange,
                    ["Failed to change imagefreeze state"]
                ));
            }
            converters.link(&imagefreeze).unwrap();
            imagefreeze.link(&queue).unwrap();
            Some(imagefreeze)
        } else {
            converters.link(&queue).unwrap();
            None
        };

        let ghostpad =
            gst::GhostPad::with_target(Some(type_), &queue.static_pad("src").unwrap()).unwrap();
        let _ = ghostpad.set_active(true);
        source.source.add_pad(&ghostpad).unwrap();

        // Link the new source pad in
        let switch_pad = switch.request_pad_simple("sink_%u").unwrap();
        switch_pad.set_property("priority", if fallback_source { 1u32 } else { 0u32 });
        ghostpad.link(&switch_pad).unwrap();

        let element_weak = element.downgrade();
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            let element = match element_weak.upgrade() {
                None => return gst::PadProbeReturn::Ok,
                Some(element) => element,
            };

            let src = element.imp();

            match info.data {
                Some(gst::PadProbeData::Event(ref ev)) if ev.type_() == gst::EventType::Eos => {
                    gst::debug!(
                        CAT,
                        obj: &element,
                        "Received EOS from {}source on pad {}",
                        if fallback_source { "fallback " } else { "" },
                        pad.name()
                    );

                    let mut state_guard = src.state.lock();
                    let state = match &mut *state_guard {
                        None => {
                            return gst::PadProbeReturn::Ok;
                        }
                        Some(state) => state,
                    };

                    if is_image {
                        gst::PadProbeReturn::Ok
                    } else if state.settings.restart_on_eos || fallback_source {
                        src.handle_source_error(&element, state, RetryReason::Eos, fallback_source);
                        drop(state_guard);
                        element.notify("statistics");

                        gst::PadProbeReturn::Drop
                    } else {
                        // Send EOS to all sinkpads of the fallbackswitch and also to the other
                        // stream's fallbackswitch if it doesn't have a main branch.
                        let mut sinkpads = vec![];

                        if let Some(stream) = {
                            if is_video {
                                state.video_stream.as_ref()
                            } else {
                                state.audio_stream.as_ref()
                            }
                        } {
                            sinkpads
                                .extend(stream.switch.sink_pads().into_iter().filter(|p| p != pad));
                        }

                        if let Some(other_stream) = {
                            if is_video {
                                state.audio_stream.as_ref()
                            } else {
                                state.video_stream.as_ref()
                            }
                        } {
                            if other_stream.main_branch.is_none() {
                                sinkpads.extend(
                                    other_stream
                                        .switch
                                        .sink_pads()
                                        .into_iter()
                                        .filter(|p| p != pad),
                                );
                            }
                        }

                        let event = ev.clone();
                        element.call_async(move |_| {
                            for sinkpad in sinkpads {
                                sinkpad.send_event(event.clone());
                            }
                        });

                        gst::PadProbeReturn::Ok
                    }
                }
                _ => gst::PadProbeReturn::Ok,
            }
        });

        let queue_srcpad = queue.static_pad("src").unwrap();
        let source_srcpad_block =
            Some(self.add_pad_probe(element, pad, &queue_srcpad, fallback_source));

        *branch_storage = Some(StreamBranch {
            source_srcpad: pad.clone(),
            source_srcpad_block,
            imagefreeze,
            clocksync,
            converters,
            queue,
            queue_srcpad,
            switch_pad,
        });

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn add_pad_probe(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
        block_pad: &gst::Pad,
        fallback_source: bool,
    ) -> Block {
        // FIXME: Not literally correct as we add the probe to the queue source pad but that's only
        // a workaround until
        //     https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/800
        // is fixed.
        gst::debug!(
            CAT,
            obj: element,
            "Adding blocking probe to pad {} for pad {} (fallback: {})",
            block_pad.name(),
            pad.name(),
            fallback_source,
        );

        let element_weak = element.downgrade();
        let probe_id = block_pad
            .add_probe(
                gst::PadProbeType::BLOCK
                    | gst::PadProbeType::BUFFER
                    | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |pad, info| {
                    let element = match element_weak.upgrade() {
                        None => return gst::PadProbeReturn::Pass,
                        Some(element) => element,
                    };
                    let pts = match info.data {
                        Some(gst::PadProbeData::Buffer(ref buffer)) => buffer.pts(),
                        Some(gst::PadProbeData::Event(ref ev)) => match ev.view() {
                            gst::EventView::Gap(ev) => Some(ev.get().0),
                            _ => return gst::PadProbeReturn::Pass,
                        },
                        _ => unreachable!(),
                    };

                    let src = element.imp();

                    if let Err(msg) = src.handle_pad_blocked(&element, pad, pts, fallback_source) {
                        element.post_error_message(msg);
                    }

                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();

        Block {
            pad: block_pad.clone(),
            probe_id,
            running_time: gst::ClockTime::NONE,
        }
    }

    fn handle_pad_blocked(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
        pts: impl Into<Option<gst::ClockTime>>,
        fallback_source: bool,
    ) -> Result<(), gst::ErrorMessage> {
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        let (branch, source) = match &mut *state {
            State {
                audio_stream:
                    Some(Stream {
                        main_branch: Some(ref mut branch),
                        ..
                    }),
                ref source,
                ..
            } if !fallback_source && &branch.queue_srcpad == pad => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Called probe on pad {} for pad {} (fallback: {})",
                    pad.name(),
                    branch.source_srcpad.name(),
                    fallback_source
                );

                (branch, source)
            }
            State {
                audio_stream:
                    Some(Stream {
                        fallback_branch: Some(ref mut branch),
                        ..
                    }),
                fallback_source: Some(ref source),
                ..
            } if fallback_source && &branch.queue_srcpad == pad => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Called probe on pad {} for pad {} (fallback: {})",
                    pad.name(),
                    branch.source_srcpad.name(),
                    fallback_source
                );

                (branch, source)
            }
            State {
                video_stream:
                    Some(Stream {
                        main_branch: Some(ref mut branch),
                        ..
                    }),
                ref source,
                ..
            } if !fallback_source && &branch.queue_srcpad == pad => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Called probe on pad {} for pad {} (fallback: {})",
                    pad.name(),
                    branch.source_srcpad.name(),
                    fallback_source,
                );

                (branch, source)
            }
            State {
                video_stream:
                    Some(Stream {
                        fallback_branch: Some(ref mut branch),
                        ..
                    }),
                fallback_source: Some(ref source),
                ..
            } if fallback_source && &branch.queue_srcpad == pad => {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Called probe on pad {} for pad {} (fallback: {})",
                    pad.name(),
                    branch.source_srcpad.name(),
                    fallback_source
                );

                (branch, source)
            }
            _ => unreachable!(),
        };

        // Directly unblock for live streams
        if source.is_live {
            if let Some(block) = branch.source_srcpad_block.take() {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Removing pad probe on pad {} for pad {} (fallback: {})",
                    pad.name(),
                    branch.source_srcpad.name(),
                    fallback_source,
                );
                block.pad.remove_probe(block.probe_id);
            }

            gst::debug!(CAT, obj: element, "Live source, unblocking directly");

            drop(state_guard);
            element.notify("status");

            return Ok(());
        }

        // Update running time for this block
        let block = match branch.source_srcpad_block {
            Some(ref mut block) => block,
            None => return Ok(()),
        };

        let segment = match pad.sticky_event::<gst::event::Segment>(0) {
            Some(ev) => ev.segment().clone(),
            None => {
                gst::warning!(CAT, obj: element, "Have no segment event yet");
                return Ok(());
            }
        };

        let segment = segment.downcast::<gst::ClockTime>().map_err(|_| {
            gst::error!(CAT, obj: element, "Have no time segment");
            gst::error_msg!(gst::CoreError::Clock, ["Have no time segment"])
        })?;

        let pts = pts.into();
        let running_time = if let Some((_, start)) =
            pts.zip(segment.start()).filter(|(pts, start)| pts < start)
        {
            segment.to_running_time(start)
        } else if let Some((_, stop)) = pts.zip(segment.stop()).filter(|(pts, stop)| pts >= stop) {
            segment.to_running_time(stop)
        } else {
            segment.to_running_time(pts)
        };

        gst::debug!(
            CAT,
            obj: element,
            "Have block running time {}",
            running_time.display(),
        );

        block.running_time = running_time;

        self.unblock_pads(element, state, fallback_source);

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn unblock_pads(&self, element: &super::FallbackSrc, state: &mut State, fallback_source: bool) {
        let current_running_time = match element.current_running_time() {
            Some(current_running_time) => current_running_time,
            None => {
                gst::debug!(CAT, obj: element, "Waiting for current_running_time");
                return;
            }
        };

        if !fallback_source && state.manually_blocked {
            gst::debug!(CAT, obj: element, "Not unblocking yet: manual unblock",);
            return;
        }

        // Check if all streams are blocked and have a running time and we have
        // 100% buffering
        if (fallback_source && state.stats.fallback_buffering_percent < 100)
            || (!fallback_source && state.stats.buffering_percent < 100)
        {
            gst::debug!(
                CAT,
                obj: element,
                "Not unblocking yet: buffering {}%",
                state.stats.buffering_percent
            );
            return;
        }

        let source = if fallback_source {
            if let Some(ref source) = state.fallback_source {
                source
            } else {
                // There are no blocked pads if there is no fallback source
                return;
            }
        } else {
            &state.source
        };

        let streams = match source.streams {
            None => {
                gst::debug!(CAT, obj: element, "Have no stream collection yet");
                return;
            }
            Some(ref streams) => streams,
        };
        let mut have_audio = false;
        let mut have_video = false;
        for stream in streams.iter() {
            have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
        }

        // For the fallback source, if we have no audio/video then that's OK and we would continue
        // using the corresponding dummy source
        let want_audio = if fallback_source {
            have_audio
        } else {
            state.settings.enable_audio
        };
        let want_video = if fallback_source {
            have_video
        } else {
            state.settings.enable_video
        };

        // FIXME: All this surely can be simplified somehow
        let mut audio_branch = state.audio_stream.as_mut().and_then(|s| {
            if fallback_source {
                s.fallback_branch.as_mut()
            } else {
                s.main_branch.as_mut()
            }
        });
        let mut video_branch = state.video_stream.as_mut().and_then(|s| {
            if fallback_source {
                s.fallback_branch.as_mut()
            } else {
                s.main_branch.as_mut()
            }
        });

        let audio_running_time = audio_branch
            .as_ref()
            .and_then(|b| b.source_srcpad_block.as_ref())
            .and_then(|b| b.running_time);
        let video_running_time = video_branch
            .as_ref()
            .and_then(|b| b.source_srcpad_block.as_ref())
            .and_then(|b| b.running_time);

        let audio_srcpad = audio_branch.as_ref().map(|b| b.source_srcpad.clone());
        let video_srcpad = video_branch.as_ref().map(|b| b.source_srcpad.clone());

        let audio_is_eos = audio_srcpad
            .as_ref()
            .map(|p| p.pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);
        let video_is_eos = video_srcpad
            .as_ref()
            .map(|p| p.pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);

        // If we need both, wait for both and take the minimum, otherwise take the one we need.
        // Also consider EOS, we'd never get a new running time after EOS so don't need to wait.
        // FIXME: All this surely can be simplified somehow

        if have_audio && want_audio && have_video && want_video {
            if audio_running_time.is_none()
                && !audio_is_eos
                && video_running_time.is_none()
                && !video_is_eos
            {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Waiting for audio and video pads to block"
                );
                return;
            } else if audio_running_time.is_none() && !audio_is_eos {
                gst::debug!(CAT, obj: element, "Waiting for audio pad to block");
                return;
            } else if video_running_time.is_none() && !video_is_eos {
                gst::debug!(CAT, obj: element, "Waiting for video pad to block");
                return;
            }

            let audio_running_time = audio_running_time.expect("checked above");
            let video_running_time = video_running_time.expect("checked above");

            let min_running_time = if audio_is_eos {
                video_running_time
            } else if video_is_eos {
                audio_running_time
            } else {
                audio_running_time.min(video_running_time)
            };

            let offset = if current_running_time > min_running_time {
                (current_running_time - min_running_time).nseconds() as i64
            } else {
                -((min_running_time - current_running_time).nseconds() as i64)
            };

            gst::debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (audio: {} eos {}, video {} eos {})",
                current_running_time,
                offset,
                audio_running_time,
                audio_is_eos,
                video_running_time,
                video_is_eos,
            );

            if let Some(block) = audio_branch
                .as_mut()
                .and_then(|b| b.source_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }

            if let Some(block) = video_branch
                .as_mut()
                .and_then(|b| b.source_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_audio && want_audio {
            let audio_running_time = match audio_running_time {
                Some(audio_running_time) => audio_running_time,
                None => {
                    gst::debug!(CAT, obj: element, "Waiting for audio pad to block");
                    return;
                }
            };

            let offset = if current_running_time > audio_running_time {
                (current_running_time - audio_running_time).nseconds() as i64
            } else {
                -((audio_running_time - current_running_time).nseconds() as i64)
            };

            gst::debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (audio: {} eos {})",
                current_running_time,
                offset,
                audio_running_time,
                audio_is_eos
            );

            if let Some(block) = audio_branch
                .as_mut()
                .and_then(|b| b.source_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_video && want_video {
            let video_running_time = match video_running_time {
                Some(video_running_time) => video_running_time,
                None => {
                    gst::debug!(CAT, obj: element, "Waiting for video pad to block");
                    return;
                }
            };

            let offset = if current_running_time > video_running_time {
                (current_running_time - video_running_time).nseconds() as i64
            } else {
                -((video_running_time - current_running_time).nseconds() as i64)
            };

            gst::debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (video: {} eos {})",
                current_running_time,
                offset,
                video_running_time,
                video_is_eos
            );

            if let Some(block) = video_branch
                .as_mut()
                .and_then(|b| b.source_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        }
    }

    fn handle_source_pad_removed(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
        fallback_source: bool,
    ) {
        gst::debug!(
            CAT,
            obj: element,
            "Pad {} removed from {}source",
            pad.name(),
            if fallback_source { "fallback " } else { "" }
        );

        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let (mut branch, is_video, source, switch) = match &mut *state {
            State {
                audio_stream:
                    Some(Stream {
                        ref mut main_branch,
                        ref switch,
                        ..
                    }),
                ref source,
                ..
            } if !fallback_source
                && main_branch.as_ref().map(|b| &b.source_srcpad) == Some(pad) =>
            {
                (main_branch.take().unwrap(), false, source, switch)
            }
            State {
                audio_stream:
                    Some(Stream {
                        ref mut fallback_branch,
                        ref switch,
                        ..
                    }),
                fallback_source: Some(ref source),
                ..
            } if fallback_source
                && fallback_branch.as_ref().map(|b| &b.source_srcpad) == Some(pad) =>
            {
                (fallback_branch.take().unwrap(), false, source, switch)
            }
            State {
                video_stream:
                    Some(Stream {
                        ref mut main_branch,
                        ref switch,
                        ..
                    }),
                ref source,
                ..
            } if !fallback_source
                && main_branch.as_ref().map(|b| &b.source_srcpad) == Some(pad) =>
            {
                (main_branch.take().unwrap(), true, source, switch)
            }
            State {
                video_stream:
                    Some(Stream {
                        ref mut fallback_branch,
                        ref switch,
                        ..
                    }),
                fallback_source: Some(ref source),
                ..
            } if fallback_source
                && fallback_branch.as_ref().map(|b| &b.source_srcpad) == Some(pad) =>
            {
                (fallback_branch.take().unwrap(), true, source, switch)
            }
            _ => return,
        };

        branch.queue.set_locked_state(true);
        let _ = branch.queue.set_state(gst::State::Null);
        source.source.remove(&branch.queue).unwrap();

        branch.converters.set_locked_state(true);
        let _ = branch.converters.set_state(gst::State::Null);
        source.source.remove(&branch.converters).unwrap();

        branch.clocksync.set_locked_state(true);
        let _ = branch.clocksync.set_state(gst::State::Null);
        source.source.remove(&branch.clocksync).unwrap();

        if let Some(imagefreeze) = branch.imagefreeze.take() {
            imagefreeze.set_locked_state(true);
            let _ = imagefreeze.set_state(gst::State::Null);
            source.source.remove(&imagefreeze).unwrap();
        }

        if branch.switch_pad.parent().as_ref() == Some(switch.upcast_ref()) {
            switch.release_request_pad(&branch.switch_pad);
        }

        let ghostpad = source
            .source
            .static_pad(if is_video { "video" } else { "audio" })
            .unwrap();
        let _ = ghostpad.set_active(false);
        source.source.remove_pad(&ghostpad).unwrap();

        self.unblock_pads(element, state, fallback_source);

        drop(state_guard);
        element.notify("status");
    }

    fn handle_buffering(&self, element: &super::FallbackSrc, m: &gst::message::Buffering) {
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let src = match m.src() {
            Some(src) => src,
            None => return,
        };

        let fallback_source = if let Some(ref source) = state.fallback_source {
            src.has_as_ancestor(&source.source)
        } else if src.has_as_ancestor(&state.source.source) {
            false
        } else {
            return;
        };

        let source = if fallback_source {
            if let Some(ref mut source) = state.fallback_source {
                source
            } else {
                return;
            }
        } else {
            &mut state.source
        };

        if source.pending_restart {
            gst::debug!(CAT, obj: element, "Has pending restart");
            return;
        }

        gst::debug!(
            CAT,
            obj: element,
            "Got buffering {}% (fallback: {})",
            m.percent(),
            fallback_source
        );

        let buffering_percent = if fallback_source {
            &mut state.stats.fallback_buffering_percent
        } else {
            &mut state.stats.buffering_percent
        };
        let last_buffering_update = if fallback_source {
            &mut state.fallback_last_buffering_update
        } else {
            &mut state.last_buffering_update
        };

        *buffering_percent = m.percent();
        if *buffering_percent < 100 {
            *last_buffering_update = Some(Instant::now());
            // Block source pads if needed to pause
            for stream in [state.audio_stream.as_mut(), state.video_stream.as_mut()]
                .iter_mut()
                .flatten()
            {
                let branch = match stream {
                    Stream {
                        main_branch: Some(ref mut branch),
                        ..
                    } if !fallback_source => branch,
                    Stream {
                        fallback_branch: Some(ref mut branch),
                        ..
                    } if fallback_source => branch,
                    _ => continue,
                };

                if branch.source_srcpad_block.is_none() {
                    branch.source_srcpad_block = Some(self.add_pad_probe(
                        element,
                        &branch.source_srcpad,
                        &branch.queue_srcpad,
                        fallback_source,
                    ));
                }
            }
        } else {
            // Check if we can unblock now
            self.unblock_pads(element, state, fallback_source);
        }

        drop(state_guard);
        element.notify("status");
        element.notify("statistics");
    }

    fn handle_streams_selected(
        &self,
        element: &super::FallbackSrc,
        m: &gst::message::StreamsSelected,
    ) {
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let src = match m.src() {
            Some(src) => src,
            None => return,
        };

        let fallback_source = if let Some(ref source) = state.fallback_source {
            src.has_as_ancestor(&source.source)
        } else if src.has_as_ancestor(&state.source.source) {
            false
        } else {
            return;
        };

        let streams = m.stream_collection();

        gst::debug!(
            CAT,
            obj: element,
            "Got stream collection {:?} (fallback: {})",
            streams.debug(),
            fallback_source,
        );

        let mut have_audio = false;
        let mut have_video = false;
        for stream in streams.iter() {
            have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
        }

        if !have_audio && state.settings.enable_audio {
            gst::warning!(
                CAT,
                obj: element,
                "Have no audio streams but audio is enabled"
            );
        }

        if !have_video && state.settings.enable_video {
            gst::warning!(
                CAT,
                obj: element,
                "Have no video streams but video is enabled"
            );
        }

        if fallback_source {
            if let Some(ref mut source) = state.fallback_source {
                source.streams = Some(streams);
            }
        } else {
            state.source.streams = Some(streams);
        }

        // This might not be the first stream collection and we might have some unblocked pads from
        // before already, which would need to be blocked again now for keeping things in sync
        for branch in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
            .iter_mut()
            .flatten()
            .filter_map(|s| {
                if fallback_source {
                    s.fallback_branch.as_mut()
                } else {
                    s.main_branch.as_mut()
                }
            })
        {
            if branch.source_srcpad_block.is_none() {
                branch.source_srcpad_block = Some(self.add_pad_probe(
                    element,
                    &branch.source_srcpad,
                    &branch.queue_srcpad,
                    fallback_source,
                ));
            }
        }

        self.unblock_pads(element, state, fallback_source);

        drop(state_guard);
        element.notify("status");
    }

    fn handle_error(&self, element: &super::FallbackSrc, m: &gst::message::Error) -> bool {
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return false;
            }
            Some(state) => state,
        };

        let src = match m.src().and_then(|s| s.downcast::<gst::Element>().ok()) {
            None => return false,
            Some(src) => src,
        };

        gst::debug!(
            CAT,
            obj: element,
            "Got error message from {}",
            src.path_string()
        );

        if src == state.source.source || src.has_as_ancestor(&state.source.source) {
            self.handle_source_error(element, state, RetryReason::Error, false);
            drop(state_guard);
            element.notify("status");
            element.notify("statistics");
            return true;
        }

        // Check if error is from fallback input and if so, use a dummy fallback
        if let Some(ref source) = state.fallback_source {
            if src == source.source || src.has_as_ancestor(&source.source) {
                self.handle_source_error(element, state, RetryReason::Error, true);
                drop(state_guard);
                element.notify("status");
                element.notify("statistics");
                return true;
            }
        }

        gst::error!(
            CAT,
            obj: element,
            "Give up for error message from {}",
            src.path_string()
        );

        false
    }

    fn handle_source_error(
        &self,
        element: &super::FallbackSrc,
        state: &mut State,
        reason: RetryReason,
        fallback_source: bool,
    ) {
        gst::debug!(
            CAT,
            obj: element,
            "Handling source error (fallback: {}): {:?}",
            fallback_source,
            reason
        );

        if fallback_source {
            state.stats.last_fallback_retry_reason = reason;
        } else {
            state.stats.last_retry_reason = reason;
        }

        let source = if fallback_source {
            state.fallback_source.as_mut().unwrap()
        } else {
            &mut state.source
        };

        if source.pending_restart {
            gst::debug!(
                CAT,
                obj: element,
                "{}source is already pending restart",
                if fallback_source { "fallback " } else { "" }
            );
            return;
        }

        // Increase retry count only if there was no pending restart
        if fallback_source {
            state.stats.num_fallback_retry += 1;
        } else {
            state.stats.num_retry += 1;
        }

        // Unschedule pending timeout, we're restarting now
        if let Some(timeout) = source.restart_timeout.take() {
            timeout.unschedule();
        }

        // Prevent state changes from changing the state in an uncoordinated way
        source.pending_restart = true;

        // Drop any EOS events from any source pads of the source that might happen because of the
        // error. We don't need to remove these pad probes because restarting the source will also
        // remove/add the pads again.
        for pad in source.source.src_pads() {
            pad.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                |_pad, info| match info.data {
                    Some(gst::PadProbeData::Event(ref event)) => {
                        if event.type_() == gst::EventType::Eos {
                            gst::PadProbeReturn::Drop
                        } else {
                            gst::PadProbeReturn::Ok
                        }
                    }
                    _ => unreachable!(),
                },
            )
            .unwrap();
        }

        let source_weak = source.source.downgrade();
        element.call_async(move |element| {
            let src = element.imp();

            let source = match source_weak.upgrade() {
                None => return,
                Some(source) => source,
            };

            // Remove blocking pad probes if they are still there as otherwise shutting down the
            // source will deadlock on the probes.
            let mut state_guard = src.state.lock();
            let state = match &mut *state_guard {
                None => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(State {
                    source:
                        SourceBin {
                            pending_restart: false,
                            ..
                        },
                    ..
                }) if !fallback_source => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(State {
                    fallback_source:
                        Some(SourceBin {
                            pending_restart: false,
                            ..
                        }),
                    ..
                }) if fallback_source => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(state) => state,
            };
            for (source_srcpad, block) in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .flatten()
                .filter_map(|s| {
                    if fallback_source {
                        s.fallback_branch.as_mut()
                    } else {
                        s.main_branch.as_mut()
                    }
                })
                .filter_map(|branch| {
                    if let Some(block) = branch.source_srcpad_block.take() {
                        Some((&branch.source_srcpad, block))
                    } else {
                        None
                    }
                })
            {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Removing pad probe for pad {}",
                    source_srcpad.name()
                );
                block.pad.remove_probe(block.probe_id);
            }
            let switch_sinkpads = [state.audio_stream.as_ref(), state.video_stream.as_ref()]
                .into_iter()
                .flatten()
                .filter_map(|s| {
                    if fallback_source {
                        s.fallback_branch.as_ref()
                    } else {
                        s.main_branch.as_ref()
                    }
                })
                .map(|branch| branch.switch_pad.clone())
                .collect::<Vec<_>>();
            drop(state_guard);

            gst::debug!(CAT, obj: element, "Flushing source");
            for pad in switch_sinkpads {
                let _ = pad.push_event(gst::event::FlushStart::builder().build());
                if let Some(switch) = pad.parent().map(|p| p.downcast::<gst::Element>().unwrap()) {
                    switch.release_request_pad(&pad);
                }
            }

            gst::debug!(
                CAT,
                obj: element,
                "Shutting down {}source",
                if fallback_source { "fallback " } else { "" }
            );
            let _ = source.set_state(gst::State::Null);

            // Sleep for 1s before retrying

            let mut state_guard = src.state.lock();
            let state = match &mut *state_guard {
                None => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(State {
                    source:
                        SourceBin {
                            pending_restart: false,
                            ..
                        },
                    ..
                }) if !fallback_source => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(State {
                    fallback_source:
                        Some(SourceBin {
                            pending_restart: false,
                            ..
                        }),
                    ..
                }) if fallback_source => {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Restarting {}source not needed anymore",
                        if fallback_source { "fallback " } else { "" }
                    );
                    return;
                }
                Some(state) => state,
            };

            for branch in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .flatten()
                .filter_map(|s| {
                    if fallback_source {
                        s.fallback_branch.as_mut()
                    } else {
                        s.main_branch.as_mut()
                    }
                })
            {
                branch.source_srcpad_block = None;
            }

            gst::debug!(CAT, obj: element, "Waiting for 1s before retrying");
            let clock = gst::SystemClock::obtain();
            let wait_time = clock.time().unwrap() + gst::ClockTime::SECOND;
            if fallback_source {
                assert!(state
                    .fallback_source
                    .as_ref()
                    .map(|s| s.pending_restart_timeout.is_none())
                    .unwrap_or(true));
            } else {
                assert!(state.source.pending_restart_timeout.is_none());
            }

            let timeout = clock.new_single_shot_id(wait_time);
            let element_weak = element.downgrade();
            timeout
                .wait_async(move |_clock, _time, _id| {
                    let element = match element_weak.upgrade() {
                        None => return,
                        Some(element) => element,
                    };

                    gst::debug!(CAT, obj: &element, "Woke up, retrying");
                    element.call_async(move |element| {
                        let src = element.imp();

                        let mut state_guard = src.state.lock();
                        let state = match &mut *state_guard {
                            None => {
                                gst::debug!(
                                    CAT,
                                    obj: element,
                                    "Restarting {}source not needed anymore",
                                    if fallback_source { "fallback " } else { "" }
                                );
                                return;
                            }
                            Some(State {
                                source:
                                    SourceBin {
                                        pending_restart: false,
                                        ..
                                    },
                                ..
                            }) if !fallback_source => {
                                gst::debug!(
                                    CAT,
                                    obj: element,
                                    "Restarting {}source not needed anymore",
                                    if fallback_source { "fallback " } else { "" }
                                );
                                return;
                            }
                            Some(State {
                                fallback_source:
                                    Some(SourceBin {
                                        pending_restart: false,
                                        ..
                                    }),
                                ..
                            }) if fallback_source => {
                                gst::debug!(
                                    CAT,
                                    obj: element,
                                    "Restarting {}source not needed anymore",
                                    if fallback_source { "fallback " } else { "" }
                                );
                                return;
                            }
                            Some(state) => state,
                        };

                        let (source, old_source) = if !fallback_source {
                            if let Source::Uri(..) = state.configured_source {
                                // FIXME: Create a new uridecodebin3 because it currently is not reusable
                                // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/746
                                element.remove(&state.source.source).unwrap();

                                let source = src.create_main_input(
                                    element,
                                    &state.configured_source,
                                    state.settings.buffer_duration,
                                );

                                (
                                    source.source.clone(),
                                    Some(mem::replace(&mut state.source, source)),
                                )
                            } else {
                                state.source.pending_restart = false;
                                state.source.pending_restart_timeout = None;
                                state.stats.buffering_percent = 100;
                                state.last_buffering_update = None;

                                if let Some(timeout) = state.source.restart_timeout.take() {
                                    gst::debug!(CAT, obj: element, "Unscheduling restart timeout");
                                    timeout.unschedule();
                                }

                                (state.source.source.clone(), None)
                            }
                        } else if let Some(ref mut source) = state.fallback_source {
                            source.pending_restart = false;
                            source.pending_restart_timeout = None;
                            state.stats.fallback_buffering_percent = 100;
                            state.fallback_last_buffering_update = None;

                            if let Some(timeout) = source.restart_timeout.take() {
                                gst::debug!(CAT, obj: element, "Unscheduling restart timeout");
                                timeout.unschedule();
                            }

                            (source.source.clone(), None)
                        } else {
                            return;
                        };

                        drop(state_guard);

                        if let Some(old_source) = old_source {
                            // Drop old source after releasing the lock, it might call the pad-removed callback
                            // still
                            drop(old_source);
                        }

                        if source.sync_state_with_parent().is_err() {
                            gst::error!(
                                CAT,
                                obj: element,
                                "{}source failed to change state",
                                if fallback_source { "fallback " } else { "" }
                            );
                            let _ = source.set_state(gst::State::Null);
                            let mut state_guard = src.state.lock();
                            let state = state_guard.as_mut().expect("no state");
                            src.handle_source_error(
                                element,
                                state,
                                RetryReason::StateChangeFailure,
                                fallback_source,
                            );
                            drop(state_guard);
                            element.notify("statistics");
                        } else {
                            let mut state_guard = src.state.lock();
                            let state = state_guard.as_mut().expect("no state");
                            if fallback_source {
                                assert!(state
                                    .fallback_source
                                    .as_ref()
                                    .map(|s| s.restart_timeout.is_none())
                                    .unwrap_or(true));
                            } else {
                                assert!(state.source.restart_timeout.is_none());
                            }
                            src.schedule_source_restart_timeout(
                                element,
                                state,
                                gst::ClockTime::ZERO,
                                fallback_source,
                            );
                        }
                    });
                })
                .expect("Failed to wait async");
            if fallback_source {
                if let Some(ref mut source) = state.fallback_source {
                    source.pending_restart_timeout = Some(timeout);
                }
            } else {
                state.source.pending_restart_timeout = Some(timeout);
            }
        });
    }

    #[allow(clippy::blocks_in_if_conditions)]
    fn schedule_source_restart_timeout(
        &self,
        element: &super::FallbackSrc,
        state: &mut State,
        elapsed: gst::ClockTime,
        fallback_source: bool,
    ) {
        if fallback_source {
            gst::fixme!(
                CAT,
                obj: element,
                "Restart timeout not implemented for fallback source"
            );
            return;
        }

        let source = if fallback_source {
            if let Some(ref mut source) = state.fallback_source {
                source
            } else {
                return;
            }
        } else {
            &mut state.source
        };

        if source.pending_restart {
            gst::debug!(
                CAT,
                obj: element,
                "Not scheduling {}source restart timeout because source is pending restart already",
                if fallback_source { "fallback " } else { "" },
            );
            return;
        }

        if source.is_image {
            gst::debug!(
                CAT,
                obj: element,
                "Not scheduling {}source restart timeout because we are playing back an image",
                if fallback_source { "fallback " } else { "" },
            );
            return;
        }

        if !fallback_source && state.manually_blocked {
            gst::debug!(
                CAT,
                obj: element,
                "Not scheduling source restart timeout because we are manually blocked",
            );
            return;
        }

        let clock = gst::SystemClock::obtain();
        let wait_time = clock.time().unwrap() + state.settings.restart_timeout - elapsed;
        gst::debug!(
            CAT,
            obj: element,
            "Scheduling {}source restart timeout for {}",
            if fallback_source { "fallback " } else { "" },
            wait_time,
        );

        let timeout = clock.new_single_shot_id(wait_time);
        let element_weak = element.downgrade();
        timeout
            .wait_async(move |_clock, _time, _id| {
                let element = match element_weak.upgrade() {
                    None => return,
                    Some(element) => element,
                };

                element.call_async(move |element| {
                    let src = element.imp();

                    gst::debug!(
                        CAT,
                        obj: element,
                        "{}source restart timeout triggered",
                        if fallback_source { "fallback " } else { "" }
                    );
                    let mut state_guard = src.state.lock();
                    let state = match &mut *state_guard {
                        None => {
                            gst::debug!(
                                CAT,
                                obj: element,
                                "Restarting {}source not needed anymore",
                                if fallback_source { "fallback " } else { "" }
                            );
                            return;
                        }
                        Some(state) => state,
                    };

                    let source = if fallback_source {
                        if let Some(ref mut source) = state.fallback_source {
                            source
                        } else {
                            return;
                        }
                    } else {
                        &mut state.source
                    };

                    source.restart_timeout = None;

                    // If we have the fallback activated then restart the source now.
                    if fallback_source || src.have_fallback_activated(element, state) {
                        let (last_buffering_update, buffering_percent) = if fallback_source {
                            (
                                state.fallback_last_buffering_update,
                                state.stats.fallback_buffering_percent,
                            )
                        } else {
                            (state.last_buffering_update, state.stats.buffering_percent)
                        };
                        // If we're not actively buffering right now let's restart the source
                        if last_buffering_update
                            .map(|i| i.elapsed() >= state.settings.restart_timeout.into())
                            .unwrap_or(buffering_percent == 100)
                        {
                            gst::debug!(
                                CAT,
                                obj: element,
                                "Not buffering, restarting {}source",
                                if fallback_source { "fallback " } else { "" }
                            );

                            src.handle_source_error(
                                element,
                                state,
                                RetryReason::Timeout,
                                fallback_source,
                            );
                            drop(state_guard);
                            element.notify("statistics");
                        } else {
                            gst::debug!(
                                CAT,
                                obj: element,
                                "Buffering, restarting {}source later",
                                if fallback_source { "fallback " } else { "" }
                            );
                            let elapsed = last_buffering_update
                                .and_then(|last_buffering_update| {
                                    gst::ClockTime::try_from(last_buffering_update.elapsed()).ok()
                                })
                                .unwrap_or(gst::ClockTime::ZERO);

                            src.schedule_source_restart_timeout(
                                element,
                                state,
                                elapsed,
                                fallback_source,
                            );
                        }
                    } else {
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Restarting {}source not needed anymore",
                            if fallback_source { "fallback " } else { "" }
                        );
                    }
                });
            })
            .expect("Failed to wait async");

        source.restart_timeout = Some(timeout);
    }

    #[allow(clippy::blocks_in_if_conditions)]
    fn have_fallback_activated(&self, _element: &super::FallbackSrc, state: &State) -> bool {
        let mut have_audio = false;
        let mut have_video = false;
        if let Some(ref streams) = state.source.streams {
            for stream in streams.iter() {
                have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
                have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
            }
        }

        // If we have neither audio nor video (no streams yet), or active pad for the ones we have
        // is the fallback pad then we have the fallback activated.
        (!have_audio && !have_video)
            || (have_audio
                && state.audio_stream.is_some()
                && state
                    .audio_stream
                    .as_ref()
                    .and_then(|s| s.switch.property::<Option<gst::Pad>>("active-pad"))
                    .map(|p| p.property::<u32>("priority") != 0)
                    .unwrap_or(true))
            || (have_video
                && state.video_stream.is_some()
                && state
                    .video_stream
                    .as_ref()
                    .and_then(|s| s.switch.property::<Option<gst::Pad>>("active-pad"))
                    .map(|p| p.property::<u32>("priority") != 0)
                    .unwrap_or(true))
    }

    fn handle_switch_active_pad_change(&self, element: &super::FallbackSrc, is_audio: bool) {
        let mut state_guard = self.state.lock();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        // If we have the fallback activated then start the retry timeout unless it was started
        // already. Otherwise cancel the retry timeout.
        if self.have_fallback_activated(element, state) {
            gst::warning!(
                CAT,
                obj: element,
                "Switched to {} fallback stream",
                if is_audio { "audio" } else { "video " }
            );
            if state.source.restart_timeout.is_none() {
                self.schedule_source_restart_timeout(element, state, gst::ClockTime::ZERO, false);
            }
        } else {
            gst::debug!(
                CAT,
                obj: element,
                "Switched to {} main stream",
                if is_audio { "audio" } else { "video" }
            );
            if let Some(timeout) = state.source.retry_timeout.take() {
                gst::debug!(CAT, obj: element, "Unscheduling retry timeout");
                timeout.unschedule();
            }

            if let Some(timeout) = state.source.restart_timeout.take() {
                gst::debug!(CAT, obj: element, "Unscheduling restart timeout");
                timeout.unschedule();
            }
        }

        drop(state_guard);
        element.notify("status");
    }

    fn stats(&self) -> gst::Structure {
        let state_guard = self.state.lock();

        let state = match &*state_guard {
            None => return Stats::default().to_structure(),
            Some(ref state) => state,
        };

        state.stats.to_structure()
    }
}
