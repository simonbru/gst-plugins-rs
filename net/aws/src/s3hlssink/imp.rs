// Copyright (C) 2022, Daily
//      Author: Arun Raghavan <arun@asymptotic.io>
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future;
use once_cell::sync::Lazy;
use std::io::Write;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Mutex;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;

use gio::prelude::{Cast, ToValue};
use gst::{element_error, glib, prelude::ObjectExt, prelude::*, subclass::prelude::*};

use aws_sdk_s3::config;
use aws_sdk_s3::model::ObjectCannedAcl;
use aws_sdk_s3::types::ByteStream;
use aws_sdk_s3::Endpoint;
use aws_sdk_s3::{Client, Credentials, Region, RetryConfig};
use aws_types::sdk_config::SdkConfig;
use http::Uri;

use crate::s3utils;

/*
 * We use a conservative channel size of 32. Using an unbounded channel or higher
 * channel size results in whole bunch of pending requests. For example, in case
 * of an unbounded channel by the time we finish uploading 10th request, 100+
 * requests might have already queued up.
 */
const S3_CHANNEL_SIZE: usize = 32;
const S3_ACL_DEFAULT: ObjectCannedAcl = ObjectCannedAcl::Private;
const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_TIMEOUT_IN_MSECS: u64 = 15000;

struct Settings {
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    s3_region: Region,
    s3_bucket: Option<String>,
    s3_key_prefix: Option<String>,
    s3_acl: ObjectCannedAcl,
    s3_upload_handle: Option<JoinHandle<()>>,
    s3_tx: Option<SyncSender<S3Request>>,
    s3_txc: Option<SyncSender<S3RequestControl>>,
    request_timeout: Duration,
    retry_attempts: u32,
    audio_sink: bool,
    video_sink: bool,
    config: Option<SdkConfig>,
    endpoint_uri: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let duration = Duration::from_millis(DEFAULT_TIMEOUT_IN_MSECS);
        Self {
            access_key: None,
            secret_access_key: None,
            session_token: None,
            s3_region: Region::new("us-west-2"),
            s3_bucket: None,
            s3_key_prefix: None,
            s3_acl: S3_ACL_DEFAULT,
            s3_upload_handle: None,
            s3_tx: None,
            s3_txc: None,
            request_timeout: duration,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            audio_sink: false,
            video_sink: false,
            config: None,
            endpoint_uri: None,
        }
    }
}

pub struct S3HlsSink {
    settings: Mutex<Settings>,
    hlssink: gst::Element,
    canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "s3hlssink",
        gst::DebugColorFlags::empty(),
        Some("S3 HLS sink"),
    )
});

#[derive(Clone)]
struct S3Upload {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
    s3_acl: ObjectCannedAcl,
    s3_tx: SyncSender<S3Request>,
    s3_data: Vec<u8>,
}

struct S3UploadReq {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
    s3_acl: ObjectCannedAcl,
    s3_data: Vec<u8>,
}

struct S3DeleteReq {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
}

enum S3Request {
    Upload(S3UploadReq),
    Delete(S3DeleteReq),
    Stop,
}

enum S3RequestControl {
    Continue,
    Pause,
}

impl S3Upload {
    fn new(
        s3_client: Client,
        settings: &Settings,
        s3_location: String,
        s3_tx: SyncSender<S3Request>,
    ) -> S3Upload {
        let s3_bucket = settings.s3_bucket.as_ref().unwrap().to_string();
        let s3_key_prefix = settings.s3_key_prefix.as_ref();
        let s3_key = if let Some(key_prefix) = s3_key_prefix {
            format!("{}/{}", key_prefix, s3_location)
        } else {
            s3_location
        };
        let s3_acl = settings.s3_acl.clone();

        S3Upload {
            s3_client,
            s3_bucket,
            s3_key,
            s3_acl,
            s3_data: Vec::new(),
            s3_tx,
        }
    }
}

impl Write for S3Upload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        gst::log!(CAT, "Write {}, {}", self.s3_key, buf.len());
        self.s3_data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for S3Upload {
    fn drop(&mut self) {
        let s3_data: Vec<u8> = self.s3_data.drain(0..).collect();
        let s3_data_len = s3_data.len();
        let s3_tx = &mut self.s3_tx;
        let s3_channel = S3UploadReq {
            s3_client: self.s3_client.clone(),
            s3_bucket: self.s3_bucket.clone(),
            s3_key: self.s3_key.clone(),
            s3_acl: self.s3_acl.clone(),
            s3_data,
        };

        gst::debug!(
            CAT,
            "Submitting upload request for key: {}",
            s3_channel.s3_key,
        );

        match s3_tx.send(S3Request::Upload(s3_channel)) {
            Ok(()) => {
                gst::debug!(
                    CAT,
                    "Send S3 key {} of data length {} succeeded",
                    self.s3_key,
                    s3_data_len,
                );
            }
            Err(_) => {
                /*
                 * A send operation can only fail if the receiving end of a
                 * channel is disconnected, implying that the data would
                 * never be received.
                 */
                gst::error!(
                    CAT,
                    "Send S3 key {} of data length {} failed",
                    self.s3_key,
                    s3_data_len,
                );
            }
        }
    }
}

fn s3_request(element: super::S3HlsSink, rxc: Receiver<S3RequestControl>, rx: Receiver<S3Request>) {
    let bin = element.imp();
    loop {
        match rxc.try_recv() {
            Ok(S3RequestControl::Continue) => (),
            Ok(S3RequestControl::Pause) => {
                gst::debug!(CAT, obj: &element, "Pausing S3 request thread.");
                match rxc.recv() {
                    Ok(S3RequestControl::Continue) => {
                        gst::debug!(CAT, obj: &element, "Continuing S3 request thread.")
                    }
                    // We do not expect another pause request here.
                    Ok(S3RequestControl::Pause) => unreachable!(),
                    Err(_) => (),
                }
            }
            /*
             * We are not concerned with `Empty` and since we close the control
             * channel ourselves when required, `Disconnected` will be expected.
             */
            Err(_) => (),
        };

        match rx.recv() {
            Ok(S3Request::Upload(data)) => {
                let s3_client = data.s3_client.clone();
                let s3_bucket = data.s3_bucket.clone();
                let s3_key = data.s3_key.clone();
                let s3_acl = data.s3_acl;
                let s3_data_len = data.s3_data.len();

                gst::debug!(CAT, obj: &element, "Uploading key {}", s3_key);

                let put_object_req = s3_client
                    .put_object()
                    .set_bucket(Some(s3_bucket))
                    .set_key(Some(s3_key.clone()))
                    .set_body(Some(ByteStream::from(data.s3_data)))
                    .set_acl(Some(s3_acl));
                let put_object_req_future = put_object_req.send();
                let result = s3utils::wait(&bin.canceller, put_object_req_future);

                if let Err(err) = result {
                    gst::error!(
                        CAT,
                        obj: &element,
                        "Put object request for S3 key {} of data length {} failed with error {:?}",
                        s3_key,
                        s3_data_len,
                        err,
                    );
                    element_error!(
                        element,
                        gst::ResourceError::Write,
                        ["Put object request failed"]
                    );
                    break;
                };
            }
            Ok(S3Request::Delete(data)) => {
                let s3_client = data.s3_client.clone();
                let s3_bucket = data.s3_bucket.clone();
                let s3_key = data.s3_key.clone();

                gst::debug!(CAT, obj: &element, "Deleting key {}", s3_key);

                let delete_object_req = s3_client
                    .delete_object()
                    .set_bucket(Some(s3_bucket))
                    .set_key(Some(s3_key.clone()));
                let delete_object_req_future = delete_object_req.send();
                let result = s3utils::wait(&bin.canceller, delete_object_req_future);

                if let Err(err) = result {
                    gst::error!(
                        CAT,
                        obj: &element,
                        "Delete object request for S3 key {} failed with error {:?}",
                        s3_key,
                        err
                    );
                    element_error!(
                        element,
                        gst::ResourceError::Write,
                        ["Delete object request failed"]
                    );
                    break;
                };
            }
            Ok(S3Request::Stop) => break,
            Err(err) => {
                gst::error!(CAT, obj: &element, "S3 channel error: {}", err);
                element_error!(element, gst::ResourceError::Write, ["S3 channel error"]);
                break;
            }
        }
    }

    gst::info!(CAT, obj: &element, "Exiting S3 request thread",);
}

fn s3client_from_settings(element: &super::S3HlsSink) -> Client {
    let bin = element.imp();
    let mut settings = bin.settings.lock().unwrap();

    if settings.config.is_none() {
        let timeout_config = s3utils::timeout_config(settings.request_timeout);
        let access_key = settings.access_key.as_ref();
        let secret_access_key = settings.secret_access_key.as_ref();
        let session_token = settings.session_token.clone();

        let cred = match (access_key, secret_access_key) {
            (Some(access), Some(secret_access)) => Some(Credentials::new(
                access,
                secret_access,
                session_token,
                None,
                "s3-hlssink",
            )),
            _ => None,
        };

        let sdk_config = s3utils::wait_config(
            &bin.canceller,
            settings.s3_region.clone(),
            timeout_config,
            cred,
        )
        .expect("Failed to get SDK config");

        settings.config = Some(sdk_config);
    }

    let sdk_config = settings.config.as_ref().expect("SDK config must be set");
    let endpoint_uri = match &settings.endpoint_uri {
        Some(endpoint) => match endpoint.parse::<Uri>() {
            Ok(uri) => Some(uri),
            Err(e) => {
                element_error!(
                    element,
                    gst::ResourceError::Settings,
                    ["Invalid S3 endpoint uri. Error: {}", e]
                );
                None
            }
        },
        None => None,
    };

    let config_builder = config::Builder::from(sdk_config)
        .region(settings.s3_region.clone())
        .retry_config(RetryConfig::standard().with_max_attempts(settings.retry_attempts));

    let config = if let Some(uri) = endpoint_uri {
        config_builder
            .endpoint_resolver(Endpoint::mutable(uri))
            .build()
    } else {
        config_builder.build()
    };

    Client::from_conf(config)
}

impl S3HlsSink {
    fn stop(&self) {
        let bin = self.instance();

        let mut settings = self.settings.lock().unwrap();
        let s3_handle = settings.s3_upload_handle.take();
        let s3_tx = settings.s3_tx.clone();

        if let (Some(handle), Some(tx)) = (s3_handle, s3_tx) {
            gst::info!(CAT, obj: &bin, "Stopping S3 request thread");
            match tx.send(S3Request::Stop) {
                Ok(_) => {
                    gst::info!(CAT, obj: &bin, "Joining S3 request thread");
                    if let Err(err) = handle.join() {
                        gst::error!(CAT, obj: &bin, "S3 upload thread failed to exit: {:?}", err);
                    }
                    drop(tx);
                }
                Err(err) => {
                    gst::error!(CAT, obj: &bin, "Failed to stop S3 request thread: {}", err)
                }
            };
        };
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3HlsSink {
    const NAME: &'static str = "S3HlsSink";
    type Type = super::S3HlsSink;
    type ParentType = gst::Bin;

    fn with_class(_klass: &Self::Class) -> Self {
        /* Prefer hlssink3 here due to it's support for media playlist types */
        let hlssink = match gst::ElementFactory::make("hlssink3", Some("hlssink3")) {
            Ok(element) => element,
            Err(_) => gst::ElementFactory::make("hlssink2", Some("hlssink2"))
                .expect("Could not find hlssink2. Need hlssink2 or hlssink3."),
        };

        Self {
            settings: Mutex::new(Settings::default()),
            hlssink,
            canceller: Mutex::new(None),
        }
    }
}

impl ObjectImpl for S3HlsSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("access-key")
                    .nick("Access Key")
                    .blurb("AWS Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-access-key")
                    .nick("Secret Access Key")
                    .blurb("AWS Secret Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-token")
                    .nick("Session Token")
                    .blurb("AWS temporary session token from STS")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("bucket")
                    .nick("S3 Bucket")
                    .blurb("The bucket of the file to write")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("key-prefix")
                    .nick("S3 key prefix")
                    .blurb("The key prefix for segment and playlist files")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("region")
                    .nick("AWS Region")
                    .blurb("The AWS region for the S3 bucket (e.g. eu-west-2).")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gst::Element>("hlssink")
                    .nick("HLS Sink")
                    .blurb("The underlying HLS sink being used")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("acl")
                    .nick("S3 ACL")
                    .blurb("Canned ACL to use for uploading to S3")
                    .default_value(Some(S3_ACL_DEFAULT.as_str()))
                    .build(),
                glib::ParamSpecUInt::builder("retry-attempts")
                    .nick("Retry attempts")
                    .blurb(
                        "Number of times AWS SDK attempts a request before abandoning the request",
                    )
                    .minimum(1)
                    .maximum(10)
                    .default_value(DEFAULT_RETRY_ATTEMPTS)
                    .build(),
                glib::ParamSpecUInt64::builder("request-timeout")
                    .nick("API call timeout")
                    .blurb("Timeout for request to S3 service (in ms)")
                    .minimum(1)
                    .default_value(DEFAULT_TIMEOUT_IN_MSECS)
                    .build(),
                glib::ParamSpecString::builder("endpoint-uri")
                    .nick("S3 endpoint URI")
                    .blurb("The S3 endpoint URI to use")
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
        let mut settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            obj: obj,
            "Setting property '{}' to '{:?}'",
            pspec.name(),
            value
        );

        match pspec.name() {
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                settings.session_token = value.get().expect("type checked upstream");
            }
            "bucket" => {
                settings.s3_bucket = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "key-prefix" => {
                settings.s3_key_prefix = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "region" => {
                let region = value.get::<String>().expect("type checked upstream");
                settings.s3_region = Region::new(region);
            }
            "acl" => {
                let s3_acl = value.get::<String>().expect("type checked upstream");
                settings.s3_acl = ObjectCannedAcl::from_str(&s3_acl).expect("Invalid ACL");
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            "request-timeout" => {
                settings.request_timeout =
                    Duration::from_millis(value.get::<u64>().expect("type checked upstream"));
            }
            "endpoint-uri" => {
                settings.endpoint_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "key-prefix" => settings.s3_key_prefix.to_value(),
            "bucket" => settings.s3_bucket.to_value(),
            "region" => settings.s3_region.to_string().to_value(),
            "hlssink" => self.hlssink.to_value(),
            "acl" => settings.s3_acl.as_str().to_value(),
            "retry-attempts" => settings.retry_attempts.to_value(),
            "request-timeout" => (settings.request_timeout.as_millis() as u64).to_value(),
            "endpoint-uri" => settings.endpoint_uri.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add(&self.hlssink).unwrap();

        let mut settings = self.settings.lock().unwrap();

        let (txc, rxc): (SyncSender<S3RequestControl>, Receiver<S3RequestControl>) =
            mpsc::sync_channel(S3_CHANNEL_SIZE);
        let (tx, rx): (SyncSender<S3Request>, Receiver<S3Request>) =
            mpsc::sync_channel(S3_CHANNEL_SIZE);

        let s3_tx = tx.clone();
        let playlist_tx = tx.clone();
        let fragment_tx = tx.clone();
        let delete_tx = tx;
        let element = obj.clone();

        let handle = spawn(move || s3_request(element, rxc, rx));

        settings.s3_upload_handle = Some(handle);
        settings.s3_tx = Some(s3_tx);
        settings.s3_txc = Some(txc);
        drop(settings);

        gst::info!(CAT, obj: obj, "Constructed");

        self.hlssink.connect("get-playlist-stream", false, {
            let element_weak = obj.downgrade();
            move |args| -> Option<glib::Value> {
                let element = match element_weak.upgrade() {
                    Some(element) => element,
                    None => return None,
                };

                let s3hlssink = element.imp();
                let s3client = s3client_from_settings(&element);
                let settings = s3hlssink.settings.lock().unwrap();

                let s3_location = args[1].get::<&str>().unwrap();
                let upload = S3Upload::new(
                    s3client,
                    &settings,
                    s3_location.to_string(),
                    playlist_tx.clone(),
                );

                gst::debug!(CAT, obj: &element, "New upload for {}", s3_location);

                Some(
                    gio::WriteOutputStream::new(upload)
                        .upcast::<gio::OutputStream>()
                        .to_value(),
                )
            }
        });

        self.hlssink.connect("get-fragment-stream", false, {
            let element_weak = obj.downgrade();
            move |args| -> Option<glib::Value> {
                let element = match element_weak.upgrade() {
                    Some(element) => element,
                    None => return None,
                };

                let s3hlssink = element.imp();
                let s3client = s3client_from_settings(&element);
                let settings = s3hlssink.settings.lock().unwrap();

                let s3_location = args[1].get::<&str>().unwrap();
                let upload = S3Upload::new(
                    s3client,
                    &settings,
                    s3_location.to_string(),
                    fragment_tx.clone(),
                );

                gst::debug!(CAT, obj: &element, "New upload for {}", s3_location);

                Some(
                    gio::WriteOutputStream::new(upload)
                        .upcast::<gio::OutputStream>()
                        .to_value(),
                )
            }
        });

        self.hlssink.connect("delete-fragment", false, {
            let element_weak = obj.downgrade();
            move |args| {
                let element = match element_weak.upgrade() {
                    Some(element) => element,
                    None => return None,
                };

                let s3hlssink = element.imp();
                let s3_client = s3client_from_settings(&element);

                let settings = s3hlssink.settings.lock().unwrap();
                let s3_bucket = settings.s3_bucket.as_ref().unwrap().clone();
                let s3_location = args[1].get::<String>().unwrap();

                let s3_key_prefix = settings.s3_key_prefix.as_ref();
                let s3_key = if let Some(key_prefix) = s3_key_prefix {
                    format!("{}/{}", key_prefix, s3_location)
                } else {
                    s3_location.to_string()
                };

                gst::debug!(CAT, obj: &element, "Deleting {}", s3_location);

                let delete = S3DeleteReq {
                    s3_client,
                    s3_bucket,
                    s3_key,
                };

                let res = delete_tx.send(S3Request::Delete(delete));

                // The signature on delete-fragment signal is different for
                // hlssink2 and hlssink3.
                if s3hlssink.hlssink.name().contains("hlssink3") {
                    if res.is_ok() {
                        Some(true.to_value())
                    } else {
                        gst::error!(CAT, obj: &element, "Failed deleting {}", s3_location);
                        element_error!(
                            element,
                            gst::ResourceError::Write,
                            ["Failed to delete fragment"]
                        );
                        Some(false.to_value())
                    }
                } else {
                    None
                }
            }
        });
    }
}

impl GstObjectImpl for S3HlsSink {}

impl ElementImpl for S3HlsSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "S3 HLS Sink",
                "Generic",
                "Streams HLS data to S3",
                "Daily. Co",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();

            let audio_sink_pad_template = gst::PadTemplate::new(
                "audio",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            let video_sink_pad_template = gst::PadTemplate::new(
                "video",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            vec![audio_sink_pad_template, video_sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(element, transition)?;
        /*
         * The settings lock must not be taken before the parent state change.
         * Parent state change will result in the callback getting called which
         * in turn will require the settings lock.
         */
        let settings = self.settings.lock().unwrap();

        match transition {
            gst::StateChange::PlayingToPaused => {
                let s3_txc = settings.s3_txc.clone();
                if let Some(tx) = s3_txc {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Sending pause request to S3 request thread."
                    );
                    if settings.s3_upload_handle.is_some()
                        && tx.send(S3RequestControl::Pause).is_err()
                    {
                        gst::error!(CAT, obj: element, "Could not send pause request.");
                    }
                }
            }
            gst::StateChange::PausedToPlaying => {
                let s3_txc = settings.s3_txc.clone();
                if let Some(tx) = s3_txc {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Sending continue request to S3 request thread."
                    );
                    if tx.send(S3RequestControl::Continue).is_err() {
                        gst::error!(CAT, obj: element, "Could not send continue request.");
                    }
                }
            }
            gst::StateChange::ReadyToNull => {
                drop(settings);
                /*
                 * Ready to Null transition will block till we finish uploading
                 * pending requests.
                 */
                self.stop();
            }
            _ => (),
        }

        Ok(ret)
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut settings = self.settings.lock().unwrap();
        match templ.name_template() {
            "audio" => {
                if settings.audio_sink {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "requested_new_pad: audio pad is already set"
                    );
                    return None;
                }

                let audio_pad = self.hlssink.request_pad_simple("audio").unwrap();
                let sink_pad =
                    gst::GhostPad::from_template_with_target(templ, Some("audio"), &audio_pad)
                        .unwrap();
                element.add_pad(&sink_pad).unwrap();
                sink_pad.set_active(true).unwrap();
                settings.audio_sink = true;

                Some(sink_pad.upcast())
            }
            "video" => {
                if settings.video_sink {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "requested_new_pad: video pad is already set"
                    );
                    return None;
                }

                let video_pad = self.hlssink.request_pad_simple("video").unwrap();
                let sink_pad =
                    gst::GhostPad::from_template_with_target(templ, Some("video"), &video_pad)
                        .unwrap();
                element.add_pad(&sink_pad).unwrap();
                sink_pad.set_active(true).unwrap();
                settings.video_sink = true;

                Some(sink_pad.upcast())
            }
            _ => {
                gst::debug!(CAT, obj: element, "requested_new_pad is not audio or video");
                None
            }
        }
    }
}

impl BinImpl for S3HlsSink {
    fn handle_message(&self, bin: &Self::Type, message: gst::Message) {
        use gst::MessageView;
        match message.view() {
            MessageView::Eos(_) | MessageView::Error(_) => {
                let mut settings = self.settings.lock().unwrap();
                let s3_txc = settings.s3_txc.take();
                if let Some(txc) = s3_txc {
                    /*
                     * A pause request would have been send to S3 request in PlayingToPause
                     * transition before ReadyToNull transition. Drop control channel here
                     * since we do not care about play pause transitions after EOS and to
                     * unblock the S3 request thread from waiting for a Continue request
                     * on the control channel.
                     */
                    gst::debug!(CAT, obj: bin, "Got EOS, dropping control channel");
                    drop(txc);
                }
                drop(settings);
                self.parent_handle_message(bin, message)
            }
            _ => self.parent_handle_message(bin, message),
        }
    }
}
