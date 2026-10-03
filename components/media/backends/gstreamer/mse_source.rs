/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The playback source for Media Source Extensions: one output pad per `SourceBuffer`.
//!
//! Sites like YouTube append audio and video to separate SourceBuffers, each its own
//! container stream with its own initialization segment. Feeding them into one byte stream
//! (as `ServoSrc` does) makes a single demuxer see two incompatible headers. Here each
//! SourceBuffer gets its own `appsrc` exposed as a "sometimes" pad, so `urisourcebin` typefinds
//! and demuxes every stream separately and `decodebin3` decodes them side by side.

use std::sync::Mutex;

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use url::Url;

mod imp {
    use std::sync::LazyLock;

    use super::*;

    #[derive(Default)]
    pub struct ServoMseSrc {
        pub(super) streams: Mutex<Vec<gstreamer_app::AppSrc>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ServoMseSrc {
        const NAME: &'static str = "ServoMseSrc";
        type Type = super::ServoMseSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);
    }

    impl ObjectImpl for ServoMseSrc {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoMseSrc {}

    impl ElementImpl for ServoMseSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Source Extensions source",
                        "Source/Audio/Video",
                        "One stream per MSE SourceBuffer",
                        "Servo developers",
                    )
                });
            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            // "Sometimes" pads: `urisourcebin` then treats the source as dynamic and handles
            // each pad as it is added, rather than only the pads present at setup.
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                vec![
                    gstreamer::PadTemplate::new(
                        "src_%u",
                        gstreamer::PadDirection::Src,
                        gstreamer::PadPresence::Sometimes,
                        &gstreamer::Caps::new_any(),
                    )
                    .unwrap(),
                ]
            });
            PAD_TEMPLATES.as_ref()
        }
    }

    impl BinImpl for ServoMseSrc {}

    impl URIHandlerImpl for ServoMseSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servomse"]
        }

        fn uri(&self) -> Option<String> {
            Some("servomse://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri) &&
                uri.scheme() == "servomse"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{uri:?}'").as_str(),
            ))
        }
    }
}

glib::wrapper! {
    pub struct ServoMseSrc(ObjectSubclass<imp::ServoMseSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMseSrc {}
unsafe impl Sync for ServoMseSrc {}

impl ServoMseSrc {
    /// Add the stream for a new SourceBuffer, returning its index.
    pub fn add_stream(&self) -> Result<usize, glib::BoolError> {
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .build()?
            .downcast::<gstreamer_app::AppSrc>()
            .expect("appsrc is an AppSrc");
        appsrc.set_format(gstreamer::Format::Bytes);
        appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);
        // A page appends ahead of playback at its own pace; never drop or block its data.
        appsrc.set_max_bytes(0);
        appsrc.set_block(false);

        let mut streams = self.imp().streams.lock().unwrap();
        let index = streams.len();
        self.add(&appsrc)?;
        appsrc.sync_state_with_parent()?;
        let template = self
            .pad_template("src_%u")
            .expect("ServoMseSrc declares src_%u");
        let pad = gstreamer::GhostPad::builder_from_template(&template)
            .name(format!("src_{index}"))
            .build();
        pad.set_target(appsrc.static_pad("src").as_ref())?;
        pad.set_active(true)?;
        self.add_pad(&pad)?;
        streams.push(appsrc);
        Ok(index)
    }

    pub fn push(
        &self,
        stream: usize,
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        let streams = self.imp().streams.lock().unwrap();
        streams[stream].push_buffer(gstreamer::Buffer::from_mut_slice(data))
    }

    /// `MediaSource.endOfStream()`: every stream has received all its data.
    pub fn end_of_stream(&self) -> Result<(), gstreamer::FlowError> {
        for stream in self.imp().streams.lock().unwrap().iter() {
            stream.end_of_stream()?;
        }
        Ok(())
    }
}

pub fn register_servo_mse_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomsesrc",
        gstreamer::Rank::NONE,
        ServoMseSrc::static_type(),
    )
}
