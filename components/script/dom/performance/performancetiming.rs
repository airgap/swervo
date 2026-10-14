/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::inheritance::Castable;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::root::DomRoot;

use super::performance::unix_epoch_time_stamp;
use crate::dom::bindings::codegen::Bindings::PerformanceTimingBinding::PerformanceTimingMethods;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// The legacy (Level 1) `performance.timing`: every attribute is an absolute time in
/// milliseconds since the Unix epoch, or 0 if it hasn't happened. Pages still compute
/// `loadEventEnd - navigationStart` from it (LinkedIn aborts its startup on a NaN there).
#[dom_struct]
pub(crate) struct PerformanceTiming {
    reflector_: Reflector,
}

impl PerformanceTiming {
    pub(crate) fn new_inherited() -> PerformanceTiming {
        PerformanceTiming {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<PerformanceTiming> {
        reflect_dom_object(cx, Box::new(PerformanceTiming::new_inherited()), global)
    }

    /// The attribute `name` in milliseconds since the Unix epoch (not `CrossProcessInstant`'s
    /// platform-specific epoch, since pages compare these with `Date.now()`), or 0 if it hasn't
    /// happened. The network timings come from the document's resource fetch timing.
    fn epoch_milliseconds(&self, name: &str) -> u64 {
        let global = self.global();
        let Some(window) = global.downcast::<Window>() else {
            return 0;
        };
        let document = window.Document();
        let instant = if name == "navigationStart" {
            document
                .navigation_timing()
                .navigation_start
                .get()
                .or_else(|| Some(window.navigation_start()))
        } else {
            document
                .performance_timing_attribute(name)
                .expect("PerformanceTiming only asks for attributes the document records")
        };
        instant.map_or(0, |instant| unix_epoch_time_stamp(instant).floor() as u64)
    }
}

impl PerformanceTimingMethods<crate::DomTypeHolder> for PerformanceTiming {
    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-navigationstart>
    fn NavigationStart(&self) -> u64 {
        self.epoch_milliseconds("navigationStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-unloadeventstart>
    fn UnloadEventStart(&self) -> u64 {
        self.epoch_milliseconds("unloadEventStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-unloadeventend>
    fn UnloadEventEnd(&self) -> u64 {
        self.epoch_milliseconds("unloadEventEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-redirectstart>
    fn RedirectStart(&self) -> u64 {
        self.epoch_milliseconds("redirectStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-redirectend>
    fn RedirectEnd(&self) -> u64 {
        self.epoch_milliseconds("redirectEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-fetchstart>
    fn FetchStart(&self) -> u64 {
        self.epoch_milliseconds("fetchStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domainlookupstart>
    fn DomainLookupStart(&self) -> u64 {
        self.epoch_milliseconds("domainLookupStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domainlookupend>
    fn DomainLookupEnd(&self) -> u64 {
        self.epoch_milliseconds("domainLookupEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-connectstart>
    fn ConnectStart(&self) -> u64 {
        self.epoch_milliseconds("connectStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-connectend>
    fn ConnectEnd(&self) -> u64 {
        self.epoch_milliseconds("connectEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-secureconnectionstart>
    fn SecureConnectionStart(&self) -> u64 {
        self.epoch_milliseconds("secureConnectionStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-requeststart>
    fn RequestStart(&self) -> u64 {
        self.epoch_milliseconds("requestStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-responsestart>
    fn ResponseStart(&self) -> u64 {
        self.epoch_milliseconds("responseStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-responseend>
    fn ResponseEnd(&self) -> u64 {
        self.epoch_milliseconds("responseEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domloading>
    fn DomLoading(&self) -> u64 {
        self.epoch_milliseconds("domLoading")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-dominteractive>
    fn DomInteractive(&self) -> u64 {
        self.epoch_milliseconds("domInteractive")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domcontentloadedeventstart>
    fn DomContentLoadedEventStart(&self) -> u64 {
        self.epoch_milliseconds("domContentLoadedEventStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domcontentloadedeventend>
    fn DomContentLoadedEventEnd(&self) -> u64 {
        self.epoch_milliseconds("domContentLoadedEventEnd")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-domcomplete>
    fn DomComplete(&self) -> u64 {
        self.epoch_milliseconds("domComplete")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-loadeventstart>
    fn LoadEventStart(&self) -> u64 {
        self.epoch_milliseconds("loadEventStart")
    }

    /// <https://w3c.github.io/navigation-timing/#dom-performancetiming-loadeventend>
    fn LoadEventEnd(&self) -> u64 {
        self.epoch_milliseconds("loadEventEnd")
    }
}
