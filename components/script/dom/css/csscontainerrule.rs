/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object_with_cx;
use servo_arc::Arc;
use style::shared_lock::{SharedRwLockReadGuard, ToCssWithGuard};
use style::stylesheets::{ContainerRule, CssRuleType};
use style_traits::ToCss;

use super::cssconditionrule::CSSConditionRule;
use super::cssrule::SpecificCSSRule;
use super::cssstylesheet::CSSStyleSheet;
use crate::dom::bindings::codegen::Bindings::CSSContainerRuleBinding::CSSContainerRuleMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/css-conditional-5/#the-csscontainerrule-interface>
#[dom_struct]
pub(crate) struct CSSContainerRule {
    css_condition_rule: CSSConditionRule,
    #[ignore_malloc_size_of = "Stylo"]
    #[no_trace]
    container_rule: RefCell<Arc<ContainerRule>>,
}

impl CSSContainerRule {
    fn new_inherited(
        parent_stylesheet: &CSSStyleSheet,
        container_rule: Arc<ContainerRule>,
    ) -> CSSContainerRule {
        let rules = container_rule.rules.clone();
        CSSContainerRule {
            css_condition_rule: CSSConditionRule::new_inherited(parent_stylesheet, rules),
            container_rule: RefCell::new(container_rule),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent_stylesheet: &CSSStyleSheet,
        container_rule: Arc<ContainerRule>,
    ) -> DomRoot<CSSContainerRule> {
        reflect_dom_object_with_cx(
            Box::new(CSSContainerRule::new_inherited(
                parent_stylesheet,
                container_rule,
            )),
            window,
            cx,
        )
    }

    /// <https://drafts.csswg.org/css-conditional-5/#dom-cssconditionrule-conditiontext>
    pub(crate) fn get_condition_text(&self) -> DOMString {
        self.container_rule
            .borrow()
            .conditions
            .to_css_string()
            .into()
    }

    pub(crate) fn update_rule(
        &self,
        container_rule: Arc<ContainerRule>,
        guard: &SharedRwLockReadGuard,
    ) {
        self.css_condition_rule
            .update_rules(container_rule.rules.clone(), guard);
        *self.container_rule.borrow_mut() = container_rule;
    }
}

impl SpecificCSSRule for CSSContainerRule {
    fn ty(&self) -> CssRuleType {
        CssRuleType::Container
    }

    fn get_css(&self) -> DOMString {
        let guard = self.css_condition_rule.shared_lock().read();
        self.container_rule.borrow().to_css_string(&guard).into()
    }
}

impl CSSContainerRuleMethods<crate::DomTypeHolder> for CSSContainerRule {
    /// <https://drafts.csswg.org/css-conditional-5/#dom-csscontainerrule-containername>
    fn ContainerName(&self) -> DOMString {
        let rule = self.container_rule.borrow();
        match rule.conditions.0.first() {
            Some(condition) => condition.name().to_css_string().into(),
            None => DOMString::new(),
        }
    }

    /// <https://drafts.csswg.org/css-conditional-5/#dom-csscontainerrule-containerquery>
    fn ContainerQuery(&self) -> DOMString {
        let rule = self.container_rule.borrow();
        match rule
            .conditions
            .0
            .first()
            .and_then(|condition| condition.query_condition())
        {
            Some(query) => query.to_css_string().into(),
            None => DOMString::new(),
        }
    }
}
