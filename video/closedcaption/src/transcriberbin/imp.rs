// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::ttutils::Cea608Mode;
use anyhow::Error;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::CaptionSource;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "transcriberbin",
        gst::DebugColorFlags::empty(),
        Some("Transcribe and inject closed captions"),
    )
});

const DEFAULT_PASSTHROUGH: bool = false;
const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(4);
const DEFAULT_ACCUMULATE: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_MODE: Cea608Mode = Cea608Mode::RollUp2;
const DEFAULT_CAPTION_SOURCE: CaptionSource = CaptionSource::Both;

struct State {
    framerate: Option<gst::Fraction>,
    tearing_down: bool,
    internal_bin: gst::Bin,
    audio_queue_passthrough: gst::Element,
    video_queue: gst::Element,
    audio_tee: gst::Element,
    transcriber_aconv: gst::Element,
    transcriber: gst::Element,
    transcriber_queue: gst::Element,
    cccombiner: gst::Element,
    transcription_bin: gst::Bin,
    textwrap: gst::Element,
    tttocea608: gst::Element,
    cccapsfilter: gst::Element,
    transcription_valve: gst::Element,
}

struct Settings {
    cc_caps: gst::Caps,
    latency: gst::ClockTime,
    passthrough: bool,
    accumulate_time: gst::ClockTime,
    mode: Cea608Mode,
    caption_source: CaptionSource,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cc_caps: gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", "raw")
                .build(),
            passthrough: DEFAULT_PASSTHROUGH,
            latency: DEFAULT_LATENCY,
            accumulate_time: DEFAULT_ACCUMULATE,
            mode: DEFAULT_MODE,
            caption_source: DEFAULT_CAPTION_SOURCE,
        }
    }
}

// Struct containing all the element data
pub struct TranscriberBin {
    audio_srcpad: gst::GhostPad,
    video_srcpad: gst::GhostPad,
    audio_sinkpad: gst::GhostPad,
    video_sinkpad: gst::GhostPad,

    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

impl TranscriberBin {
    fn construct_transcription_bin(
        &self,
        element: &super::TranscriberBin,
        state: &mut State,
    ) -> Result<(), Error> {
        gst::debug!(CAT, obj: element, "Building transcription bin");

        let aqueue_transcription = gst::ElementFactory::make("queue", Some("transqueue"))?;
        aqueue_transcription.set_property("max-size-buffers", 0u32);
        aqueue_transcription.set_property("max-size-bytes", 0u32);
        aqueue_transcription.set_property("max-size-time", 5_000_000_000u64);
        aqueue_transcription.set_property_from_str("leaky", "downstream");
        let ccconverter = gst::ElementFactory::make("ccconverter", None)?;

        state.transcription_bin.add_many(&[
            &aqueue_transcription,
            &state.transcriber_aconv,
            &state.transcriber,
            &state.transcriber_queue,
            &state.textwrap,
            &state.tttocea608,
            &ccconverter,
            &state.cccapsfilter,
            &state.transcription_valve,
        ])?;

        gst::Element::link_many(&[
            &aqueue_transcription,
            &state.transcriber_aconv,
            &state.transcriber,
            &state.transcriber_queue,
            &state.textwrap,
            &state.tttocea608,
            &ccconverter,
            &state.cccapsfilter,
            &state.transcription_valve,
        ])?;

        let transcription_audio_sinkpad = gst::GhostPad::with_target(
            Some("sink"),
            &aqueue_transcription.static_pad("sink").unwrap(),
        )?;
        let transcription_audio_srcpad = gst::GhostPad::with_target(
            Some("src"),
            &state.transcription_valve.static_pad("src").unwrap(),
        )?;

        state
            .transcription_bin
            .add_pad(&transcription_audio_sinkpad)?;
        state
            .transcription_bin
            .add_pad(&transcription_audio_srcpad)?;

        state
            .transcriber_queue
            .set_property("max-size-buffers", 0u32);
        state.transcriber_queue.set_property("max-size-time", 0u64);

        state.internal_bin.add(&state.transcription_bin)?;

        state.textwrap.set_property("lines", 2u32);

        state.transcription_bin.set_locked_state(true);

        Ok(())
    }

    fn construct_internal_bin(
        &self,
        element: &super::TranscriberBin,
        state: &mut State,
    ) -> Result<(), Error> {
        let aclocksync = gst::ElementFactory::make("clocksync", None)?;

        let vclocksync = gst::ElementFactory::make("clocksync", None)?;

        state.internal_bin.add_many(&[
            &aclocksync,
            &state.audio_tee,
            &state.audio_queue_passthrough,
            &vclocksync,
            &state.video_queue,
            &state.cccombiner,
        ])?;

        aclocksync.link(&state.audio_tee)?;
        state
            .audio_tee
            .link_pads(Some("src_%u"), &state.audio_queue_passthrough, Some("sink"))?;
        vclocksync.link(&state.video_queue)?;
        state
            .video_queue
            .link_pads(Some("src"), &state.cccombiner, Some("sink"))?;

        let internal_audio_sinkpad = gst::GhostPad::with_target(
            Some("audio_sink"),
            &aclocksync.static_pad("sink").unwrap(),
        )?;
        let internal_audio_srcpad = gst::GhostPad::with_target(
            Some("audio_src"),
            &state.audio_queue_passthrough.static_pad("src").unwrap(),
        )?;
        let internal_video_sinkpad = gst::GhostPad::with_target(
            Some("video_sink"),
            &vclocksync.static_pad("sink").unwrap(),
        )?;
        let internal_video_srcpad = gst::GhostPad::with_target(
            Some("video_src"),
            &state.cccombiner.static_pad("src").unwrap(),
        )?;

        state.internal_bin.add_pad(&internal_audio_sinkpad)?;
        state.internal_bin.add_pad(&internal_audio_srcpad)?;
        state.internal_bin.add_pad(&internal_video_sinkpad)?;
        state.internal_bin.add_pad(&internal_video_srcpad)?;

        let element_weak = element.downgrade();
        let comp_sinkpad = &state.cccombiner.static_pad("sink").unwrap();
        // Drop caption meta from video buffer if user preference is transcription
        comp_sinkpad.add_probe(gst::PadProbeType::BUFFER, move |_, probe_info| {
            let element = match element_weak.upgrade() {
                None => return gst::PadProbeReturn::Remove,
                Some(element) => element,
            };

            let trans = TranscriberBin::from_instance(&element);
            let settings = trans.settings.lock().unwrap();
            if settings.caption_source != CaptionSource::Transcription {
                return gst::PadProbeReturn::Pass;
            }

            if let Some(gst::PadProbeData::Buffer(buffer)) = &mut probe_info.data {
                let buffer = buffer.make_mut();
                while let Some(meta) = buffer.meta_mut::<gst_video::VideoCaptionMeta>() {
                    meta.remove().unwrap();
                }
            }

            gst::PadProbeReturn::Ok
        });

        element.add(&state.internal_bin)?;

        state
            .cccombiner
            .set_property("latency", 100 * gst::ClockTime::MSECOND);

        self.audio_sinkpad
            .set_target(Some(&state.internal_bin.static_pad("audio_sink").unwrap()))?;
        self.audio_srcpad
            .set_target(Some(&state.internal_bin.static_pad("audio_src").unwrap()))?;
        self.video_sinkpad
            .set_target(Some(&state.internal_bin.static_pad("video_sink").unwrap()))?;
        self.video_srcpad
            .set_target(Some(&state.internal_bin.static_pad("video_src").unwrap()))?;

        self.construct_transcription_bin(element, state)?;

        Ok(())
    }

    fn setup_transcription(&self, element: &super::TranscriberBin, state: &State) {
        let settings = self.settings.lock().unwrap();
        let mut cc_caps = settings.cc_caps.clone();

        let cc_caps_mut = cc_caps.make_mut();
        let s = cc_caps_mut.structure_mut(0).unwrap();

        s.set("framerate", &state.framerate.unwrap());

        state.cccapsfilter.set_property("caps", &cc_caps);

        let max_size_time = settings.latency + settings.accumulate_time;

        for queue in &[&state.audio_queue_passthrough, &state.video_queue] {
            queue.set_property("max-size-bytes", 0u32);
            queue.set_property("max-size-buffers", 0u32);
            queue.set_property("max-size-time", max_size_time);
        }

        let latency_ms = settings.latency.mseconds() as u32;
        state.transcriber.set_property("latency", latency_ms);

        if !settings.passthrough {
            let audio_tee_pad = state.audio_tee.request_pad_simple("src_%u").unwrap();
            let transcription_sink_pad = state.transcription_bin.static_pad("sink").unwrap();
            audio_tee_pad.link(&transcription_sink_pad).unwrap();

            state
                .transcription_bin
                .link_pads(Some("src"), &state.cccombiner, Some("caption"))
                .unwrap();

            state.transcription_bin.set_locked_state(false);
            state.transcription_bin.sync_state_with_parent().unwrap();
        }

        drop(settings);

        self.setup_cc_mode(element, state);
    }

    fn disable_transcription_bin(&self, element: &super::TranscriberBin) {
        let mut state = self.state.lock().unwrap();

        if let Some(ref mut state) = state.as_mut() {
            state.tearing_down = false;

            // At this point, we want to check whether passthrough
            // has been unset in the meantime
            let passthrough = self.settings.lock().unwrap().passthrough;

            if passthrough {
                gst::debug!(CAT, obj: element, "disabling transcription bin");

                let bin_sink_pad = state.transcription_bin.static_pad("sink").unwrap();
                if let Some(audio_tee_pad) = bin_sink_pad.peer() {
                    audio_tee_pad.unlink(&bin_sink_pad).unwrap();
                    state.audio_tee.release_request_pad(&audio_tee_pad);
                }

                let bin_src_pad = state.transcription_bin.static_pad("src").unwrap();
                if let Some(cccombiner_pad) = bin_src_pad.peer() {
                    bin_src_pad.unlink(&cccombiner_pad).unwrap();
                    state.cccombiner.release_request_pad(&cccombiner_pad);
                }

                state.transcription_bin.set_locked_state(true);
                state.transcription_bin.set_state(gst::State::Null).unwrap();
            }
        }
    }

    fn block_and_update(&self, element: &super::TranscriberBin, passthrough: bool) {
        let mut s = self.state.lock().unwrap();

        if let Some(ref mut state) = s.as_mut() {
            if passthrough {
                let sinkpad = state.transcription_bin.static_pad("sink").unwrap();
                let element_weak = element.downgrade();
                state.tearing_down = true;
                drop(s);
                let _ = sinkpad.add_probe(
                    gst::PadProbeType::IDLE
                        | gst::PadProbeType::BUFFER
                        | gst::PadProbeType::EVENT_DOWNSTREAM,
                    move |_pad, _info| {
                        let element = match element_weak.upgrade() {
                            None => return gst::PadProbeReturn::Pass,
                            Some(element) => element,
                        };

                        let this = element.imp();

                        this.disable_transcription_bin(&element);

                        gst::PadProbeReturn::Remove
                    },
                );
            } else if state.tearing_down {
                // Do nothing, wait for the previous transcription bin
                // to finish tearing down
            } else {
                state
                    .transcription_bin
                    .link_pads(Some("src"), &state.cccombiner, Some("caption"))
                    .unwrap();
                state.transcription_bin.set_locked_state(false);
                state.transcription_bin.sync_state_with_parent().unwrap();

                let audio_tee_pad = state.audio_tee.request_pad_simple("src_%u").unwrap();
                let transcription_sink_pad = state.transcription_bin.static_pad("sink").unwrap();
                audio_tee_pad.link(&transcription_sink_pad).unwrap();
            }
        }
    }

    fn setup_cc_mode(&self, element: &super::TranscriberBin, state: &State) {
        let mode = self.settings.lock().unwrap().mode;

        gst::debug!(CAT, obj: element, "setting CC mode {:?}", mode);

        state.tttocea608.set_property("mode", mode);

        if mode.is_rollup() {
            state.textwrap.set_property("accumulate-time", 0u64);
        } else {
            let accumulate_time = self.settings.lock().unwrap().accumulate_time;

            state
                .textwrap
                .set_property("accumulate-time", accumulate_time);
        }
    }

    /* We make no ceremonies here because the function can only
     * be called in READY */
    fn relink_transcriber(
        &self,
        state: &mut State,
        element: &super::TranscriberBin,
        old_transcriber: &gst::Element,
    ) -> Result<(), Error> {
        gst::error!(
            CAT,
            obj: element,
            "Relinking transcriber, old: {:?}, new: {:?}",
            old_transcriber,
            state.transcriber
        );

        state.transcriber_aconv.unlink(old_transcriber);
        old_transcriber.unlink(&state.transcriber_queue);
        state.transcription_bin.remove(old_transcriber).unwrap();
        old_transcriber.set_state(gst::State::Null).unwrap();

        state.transcription_bin.add(&state.transcriber)?;
        state.transcriber.sync_state_with_parent().unwrap();
        gst::Element::link_many(&[
            &state.transcriber_aconv,
            &state.transcriber,
            &state.transcriber_queue,
        ])?;

        Ok(())
    }

    #[allow(clippy::single_match)]
    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::TranscriberBin,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut upstream_query = gst::query::Latency::new();

                let ret = pad.query_default(Some(element), &mut upstream_query);

                if ret {
                    let (_, mut min, _) = upstream_query.result();
                    let received_framerate = {
                        let state = self.state.lock().unwrap();
                        if let Some(state) = state.as_ref() {
                            state.framerate.is_some()
                        } else {
                            false
                        }
                    };

                    let settings = self.settings.lock().unwrap();
                    if settings.passthrough || !received_framerate {
                        min += settings.latency + settings.accumulate_time;
                    } else if settings.mode.is_rollup() {
                        min += settings.accumulate_time;
                    }

                    q.set(true, min, gst::ClockTime::NONE);
                }

                ret
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn build_state(&self) -> Result<State, Error> {
        let internal_bin = gst::Bin::new(Some("internal"));
        let transcription_bin = gst::Bin::new(Some("transcription-bin"));
        let audio_tee = gst::ElementFactory::make("tee", None)?;
        let cccombiner = gst::ElementFactory::make("cccombiner", Some("cccombiner"))?;
        let textwrap = gst::ElementFactory::make("textwrap", Some("textwrap"))?;
        let tttocea608 = gst::ElementFactory::make("tttocea608", Some("tttocea608"))?;
        let transcriber_aconv = gst::ElementFactory::make("audioconvert", None)?;
        let transcriber = gst::ElementFactory::make("awstranscriber", Some("transcriber"))?;
        let transcriber_queue = gst::ElementFactory::make("queue", None)?;
        let audio_queue_passthrough = gst::ElementFactory::make("queue", None)?;
        let video_queue = gst::ElementFactory::make("queue", None)?;
        let cccapsfilter = gst::ElementFactory::make("capsfilter", None)?;
        let transcription_valve = gst::ElementFactory::make("valve", None)?;

        // Protect passthrough enable (and resulting dynamic reconfigure)
        // from non-streaming thread
        audio_tee.set_property("allow-not-linked", true);
        transcription_valve.set_property_from_str("drop-mode", "transform-to-gap");

        Ok(State {
            framerate: None,
            internal_bin,
            audio_queue_passthrough,
            video_queue,
            transcriber_aconv,
            transcriber,
            transcriber_queue,
            audio_tee,
            cccombiner,
            transcription_bin,
            textwrap,
            tttocea608,
            cccapsfilter,
            transcription_valve,
            tearing_down: false,
        })
    }

    #[allow(clippy::single_match)]
    fn video_sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::TranscriberBin,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(e) => {
                let mut state = self.state.lock().unwrap();

                if let Some(ref mut state) = state.as_mut() {
                    let caps = e.caps();
                    let s = caps.structure(0).unwrap();

                    let had_framerate = state.framerate.is_some();

                    if let Ok(framerate) = s.get::<gst::Fraction>("framerate") {
                        state.framerate = Some(framerate);
                    } else {
                        state.framerate = Some(gst::Fraction::new(30, 1));
                    }

                    if !had_framerate {
                        gst::info!(
                            CAT,
                            obj: element,
                            "Received video caps, setting up transcription"
                        );
                        self.setup_transcription(element, state);
                    }
                }

                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TranscriberBin {
    const NAME: &'static str = "RsTranscriberBin";
    type Type = super::TranscriberBin;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink_audio").unwrap();
        let audio_sinkpad = gst::GhostPad::from_template(&templ, Some("sink_audio"));
        let templ = klass.pad_template("src_audio").unwrap();
        let audio_srcpad = gst::GhostPad::builder_with_template(&templ, Some("src_audio"))
            .query_function(|pad, parent, query| {
                TranscriberBin::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.src_query(pad.upcast_ref(), element, query),
                )
            })
            .build();

        let templ = klass.pad_template("sink_video").unwrap();
        let video_sinkpad = gst::GhostPad::builder_with_template(&templ, Some("sink_video"))
            .event_function(|pad, parent, event| {
                TranscriberBin::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| {
                        transcriber.video_sink_event(pad.upcast_ref(), element, event)
                    },
                )
            })
            .build();
        let templ = klass.pad_template("src_video").unwrap();
        let video_srcpad = gst::GhostPad::builder_with_template(&templ, Some("src_video"))
            .query_function(|pad, parent, query| {
                TranscriberBin::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.src_query(pad.upcast_ref(), element, query),
                )
            })
            .build();

        Self {
            audio_srcpad,
            video_srcpad,
            audio_sinkpad,
            video_sinkpad,
            state: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TranscriberBin {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("passthrough")
                    .nick("Passthrough")
                    .blurb("Whether transcription should occur")
                    .default_value(DEFAULT_PASSTHROUGH)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow the transcriber")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("accumulate-time")
                    .nick("accumulate-time")
                    .blurb("Cut-off time for textwrap accumulation, in milliseconds (0=do not accumulate). \
                    Set this to a non-default value if you plan to switch to pop-on mode")
                    .default_value(DEFAULT_ACCUMULATE.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<Cea608Mode>("mode", DEFAULT_MODE)
                    .nick("Mode")
                    .blurb("Which closed caption mode to operate in")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("cc-caps")
                    .nick("Closed Caption caps")
                    .blurb("The expected format of the closed captions")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gst::Element>("transcriber")
                    .nick("Transcriber")
                    .blurb("The transcriber element to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<CaptionSource>("caption-source", DEFAULT_CAPTION_SOURCE)
                    .nick("Caption source")
                    .blurb("Caption source to use. \
                    If \"Transcription\" or \"Inband\" is selected, the caption meta \
                    of the other source will be dropped by transcriberbin")
                    .mutable_playing()
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
            "passthrough" => {
                let mut settings = self.settings.lock().unwrap();

                let old_passthrough = settings.passthrough;
                let new_passthrough = value.get().expect("type checked upstream");
                settings.passthrough = new_passthrough;

                if old_passthrough != new_passthrough {
                    drop(settings);
                    self.block_and_update(obj, new_passthrough);
                }
            }
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "accumulate-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.accumulate_time = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "mode" => {
                let mut settings = self.settings.lock().unwrap();

                let old_mode = settings.mode;
                let new_mode = value.get().expect("type checked upstream");
                settings.mode = new_mode;

                if old_mode != new_mode {
                    drop(settings);
                    self.setup_cc_mode(obj, self.state.lock().unwrap().as_ref().unwrap());
                }
            }
            "cc-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_caps = value.get().expect("type checked upstream");
            }
            "transcriber" => {
                let mut s = self.state.lock().unwrap();
                if let Some(ref mut state) = s.as_mut() {
                    let old_transcriber = state.transcriber.clone();
                    state.transcriber = value.get().expect("type checked upstream");
                    if old_transcriber != state.transcriber {
                        match self.relink_transcriber(state, obj, &old_transcriber) {
                            Ok(()) => (),
                            Err(err) => {
                                gst::error!(CAT, "invalid transcriber: {}", err);
                                drop(s);
                                *self.state.lock().unwrap() = None;
                            }
                        }
                    }
                }
            }
            "caption-source" => {
                let mut settings = self.settings.lock().unwrap();
                settings.caption_source = value.get().expect("type checked upstream");

                let s = self.state.lock().unwrap();
                if let Some(state) = s.as_ref() {
                    if settings.caption_source == CaptionSource::Inband {
                        gst::debug!(CAT, obj: obj, "Use inband caption, dropping transcription");
                        state.transcription_valve.set_property("drop", true);
                    } else {
                        gst::debug!(CAT, obj: obj, "Stop dropping transcription");
                        state.transcription_valve.set_property("drop", false);
                    }
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "passthrough" => {
                let settings = self.settings.lock().unwrap();
                settings.passthrough.to_value()
            }
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "accumulate-time" => {
                let settings = self.settings.lock().unwrap();
                (settings.accumulate_time.mseconds() as u32).to_value()
            }
            "mode" => {
                let settings = self.settings.lock().unwrap();
                settings.mode.to_value()
            }
            "cc-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_caps.to_value()
            }
            "transcriber" => {
                let state = self.state.lock().unwrap();
                if let Some(state) = state.as_ref() {
                    state.transcriber.to_value()
                } else {
                    let ret: Option<gst::Element> = None;
                    ret.to_value()
                }
            }
            "caption-source" => {
                let settings = self.settings.lock().unwrap();
                settings.caption_source.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.audio_srcpad).unwrap();
        obj.add_pad(&self.audio_sinkpad).unwrap();
        obj.add_pad(&self.video_srcpad).unwrap();
        obj.add_pad(&self.video_sinkpad).unwrap();

        *self.state.lock().unwrap() = match self.build_state() {
            Ok(mut state) => match self.construct_internal_bin(obj, &mut state) {
                Ok(()) => Some(state),
                Err(err) => {
                    gst::error!(CAT, "Failed to build internal bin: {}", err);
                    None
                }
            },
            Err(err) => {
                gst::error!(CAT, "Failed to build state: {}", err);
                None
            }
        }
    }
}

impl GstObjectImpl for TranscriberBin {}

impl ElementImpl for TranscriberBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "TranscriberBin",
                "Audio / Video / Text",
                "Transcribes audio and adds it as closed captions",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("video/x-raw").build();
            let video_src_pad_template = gst::PadTemplate::new(
                "src_video",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let video_sink_pad_template = gst::PadTemplate::new(
                "sink_video",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("audio/x-raw").build();
            let audio_src_pad_template = gst::PadTemplate::new(
                "src_audio",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let audio_sink_pad_template = gst::PadTemplate::new(
                "sink_audio",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![
                video_src_pad_template,
                video_sink_pad_template,
                audio_src_pad_template,
                audio_sink_pad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();

                if let Some(ref mut state) = state.as_mut() {
                    if state.framerate.is_some() {
                        gst::info!(
                            CAT,
                            obj: element,
                            "Received video caps, setting up transcription"
                        );
                        self.setup_transcription(element, state);
                    }
                } else {
                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["Can't change state with no state"]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

impl BinImpl for TranscriberBin {
    fn handle_message(&self, bin: &Self::Type, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Error(m) => {
                /* We must have a state here */
                let s = self.state.lock().unwrap();

                if let Some(state) = s.as_ref() {
                    if msg.src().as_ref() == Some(state.transcriber.upcast_ref()) {
                        gst::error!(
                            CAT,
                            obj: bin,
                            "Transcriber has posted an error ({:?}), going back to passthrough",
                            m
                        );
                        drop(s);
                        let mut settings = self.settings.lock().unwrap();
                        settings.passthrough = true;
                        drop(settings);
                        bin.notify("passthrough");
                        bin.call_async(move |bin| {
                            let thiz = bin.imp();
                            thiz.block_and_update(bin, true);
                        });
                    } else {
                        drop(s);
                        self.parent_handle_message(bin, msg);
                    }
                } else {
                    drop(s);
                    self.parent_handle_message(bin, msg);
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}
