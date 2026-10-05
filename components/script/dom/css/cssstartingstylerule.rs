/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object_with_cx;
use servo_arc::Arc;
use style::shared_lock::{Locked, SharedRwLockReadGuard, ToCssWithGuard};
use style::stylesheets::{CssRuleType, CssRules, StartingStyleRule};

use super::cssgroupingrule::CSSGroupingRule;
use super::cssrule::SpecificCSSRule;
use super::cssstylesheet::CSSStyleSheet;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct CSSStartingStyleRule {
    css_grouping_rule: CSSGroupingRule,
    #[ignore_malloc_size_of = "Stylo"]
    #[no_trace]
    starting_style_rule: RefCell<Arc<StartingStyleRule>>,
}

impl CSSStartingStyleRule {
    pub(crate) fn new_inherited(
        parent_stylesheet: &CSSStyleSheet,
        starting_style_rule: Arc<StartingStyleRule>,
    ) -> CSSStartingStyleRule {
        CSSStartingStyleRule {
            css_grouping_rule: CSSGroupingRule::new_inherited(parent_stylesheet),
            starting_style_rule: RefCell::new(starting_style_rule),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent_stylesheet: &CSSStyleSheet,
        starting_style_rule: Arc<StartingStyleRule>,
    ) -> DomRoot<CSSStartingStyleRule> {
        reflect_dom_object_with_cx(
            Box::new(CSSStartingStyleRule::new_inherited(
                parent_stylesheet,
                starting_style_rule,
            )),
            window,
            cx,
        )
    }

    pub(crate) fn clone_rules(&self) -> Arc<Locked<CssRules>> {
        self.starting_style_rule.borrow().rules.clone()
    }

    pub(crate) fn update_rule(
        &self,
        starting_style_rule: Arc<StartingStyleRule>,
        guard: &SharedRwLockReadGuard,
    ) {
        self.css_grouping_rule
            .update_rules(&starting_style_rule.rules, guard);
        *self.starting_style_rule.borrow_mut() = starting_style_rule;
    }
}

impl SpecificCSSRule for CSSStartingStyleRule {
    fn ty(&self) -> CssRuleType {
        CssRuleType::StartingStyle
    }

    fn get_css(&self) -> DOMString {
        let guard = self.css_grouping_rule.shared_lock().read();
        self.starting_style_rule
            .borrow()
            .to_css_string(&guard)
            .into()
    }
}
