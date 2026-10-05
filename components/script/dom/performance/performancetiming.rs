/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use super::performance::unix_epoch_time_stamp;
use crate::dom::bindings::codegen::Bindings::PerformanceTimingBinding::PerformanceTimingMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::Window_Binding::WindowMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::script_runtime::CanGc;

/// The legacy (Level 1) `performance.timing`: every attribute is an absolute time in
/// milliseconds since the Unix epoch, or 0 if it hasn't happened. Pages still compute
/// `loadEventEnd - navigationStart` from it (LinkedIn aborts its startup on a NaN there).
#[dom_struct]
pub(crate) struct PerformanceTiming {
    reflector_: Reflector,
}

impl PerformanceTiming {
    fn new_inherited() -> PerformanceTiming {
        PerformanceTiming {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<PerformanceTiming> {
        reflect_dom_object(Box::new(PerformanceTiming::new_inherited()), global, can_gc)
    }

    fn epoch_milliseconds(&self, name: &str) -> u64 {
        let window = self.global();
        let window = window.as_window();
        let instant = if name == "navigationStart" {
            Some(window.Performance().time_origin())
        } else {
            window
                .Document()
                .performance_timing_attribute(name)
                .expect("PerformanceTiming only asks for attributes the document records")
        };
        instant.map_or(0, |instant| unix_epoch_time_stamp(instant).floor() as u64)
    }
}

impl PerformanceTimingMethods<crate::DomTypeHolder> for PerformanceTiming {
    fn NavigationStart(&self) -> u64 {
        self.epoch_milliseconds("navigationStart")
    }

    fn UnloadEventStart(&self) -> u64 {
        self.epoch_milliseconds("unloadEventStart")
    }

    fn UnloadEventEnd(&self) -> u64 {
        self.epoch_milliseconds("unloadEventEnd")
    }

    fn RedirectStart(&self) -> u64 {
        self.epoch_milliseconds("redirectStart")
    }

    fn RedirectEnd(&self) -> u64 {
        self.epoch_milliseconds("redirectEnd")
    }

    fn FetchStart(&self) -> u64 {
        self.epoch_milliseconds("fetchStart")
    }

    fn DomainLookupStart(&self) -> u64 {
        self.epoch_milliseconds("domainLookupStart")
    }

    fn DomainLookupEnd(&self) -> u64 {
        self.epoch_milliseconds("domainLookupEnd")
    }

    fn ConnectStart(&self) -> u64 {
        self.epoch_milliseconds("connectStart")
    }

    fn ConnectEnd(&self) -> u64 {
        self.epoch_milliseconds("connectEnd")
    }

    fn SecureConnectionStart(&self) -> u64 {
        self.epoch_milliseconds("secureConnectionStart")
    }

    fn RequestStart(&self) -> u64 {
        self.epoch_milliseconds("requestStart")
    }

    fn ResponseStart(&self) -> u64 {
        self.epoch_milliseconds("responseStart")
    }

    fn ResponseEnd(&self) -> u64 {
        self.epoch_milliseconds("responseEnd")
    }

    fn DomLoading(&self) -> u64 {
        self.epoch_milliseconds("domLoading")
    }

    fn DomInteractive(&self) -> u64 {
        self.epoch_milliseconds("domInteractive")
    }

    fn DomContentLoadedEventStart(&self) -> u64 {
        self.epoch_milliseconds("domContentLoadedEventStart")
    }

    fn DomContentLoadedEventEnd(&self) -> u64 {
        self.epoch_milliseconds("domContentLoadedEventEnd")
    }

    fn DomComplete(&self) -> u64 {
        self.epoch_milliseconds("domComplete")
    }

    fn LoadEventStart(&self) -> u64 {
        self.epoch_milliseconds("loadEventStart")
    }

    fn LoadEventEnd(&self) -> u64 {
        self.epoch_milliseconds("loadEventEnd")
    }
}
