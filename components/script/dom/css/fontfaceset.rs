/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use app_units::Au;
use dom_struct::dom_struct;
use fonts::{FontDescriptor, FontFaceRuleInfo, FontTemplateDescriptor, LowercaseFontFamilyName};
use js::context::JSContext;
use js::gc::Handle;
use js::jsapi::Value;
use js::jsval::ObjectValue;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use layout_api::{QueryMsg, ReflowGoal};
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::FontFaceBinding::{
    FontFaceLoadStatus, FontFaceMethods,
};
use script_bindings::like::Setlike;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_arc::Arc as ServoArc;
use style::computed_values::font_optical_sizing::T as FontOpticalSizing;
use style::computed_values::font_variant_caps::T as FontVariantCaps;
use style::properties::{
    PropertyDeclaration, PropertyId, ShorthandId, SourcePropertyDeclaration,
    parse_one_declaration_into,
};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::computed::font::SingleFontFamily;
use style::values::computed::{FontStyle, FontSynthesis, FontWeight, FontWidth};
use style::values::generics::font::FontStyle as GenericFontStyle;
use style::values::specified::font::{
    FONT_STYLE_OBLIQUE_MAX_ANGLE_DEGREES, FONT_STYLE_OBLIQUE_MIN_ANGLE_DEGREES,
    FontFamily as SpecifiedFontFamily, FontStyle as SpecifiedFontStyleProperty,
    FontWeight as SpecifiedFontWeight, FontWidth as SpecifiedFontWidth,
};
use style_traits::ParsingMode;

use crate::dom::bindings::codegen::Bindings::FontFaceSetBinding::FontFaceSetMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::fontface::FontFace;
use crate::dom::globalscope::GlobalScope;
use crate::dom::node::NodeTraits;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise, wait_for_all_promise};
use crate::dom::promisenativehandler::Callback;
use crate::dom::types::PromiseNativeHandler;
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;

/// The result of <https://drafts.csswg.org/css-font-loading/#find-the-matching-font-faces>.
struct MatchingFontFaces {
    font_faces: Vec<DomRoot<FontFace>>,
    /// The named families of the `font` argument. The status of CSS-connected [`FontFace`]s is
    /// not updated when their `@font-face` rule loads, so callers consult the font context about
    /// whether stylesheet fonts of these families are still loading.
    family_names: Vec<LowercaseFontFamilyName>,
}

/// Parse a `font` shorthand value into its font families and the style that font matching uses,
/// with relative values absolutized against the initial values, as in step 1 of
/// <https://drafts.csswg.org/css-font-loading/#find-the-matching-font-faces>.
fn parse_font_for_matching(
    window: &Window,
    font: &str,
) -> Fallible<(Vec<SingleFontFamily>, FontDescriptor)> {
    let document = window.Document();
    let url_data = UrlExtraData(document.owner_global().api_base_url().get_arc());
    let mut declarations = SourcePropertyDeclaration::default();
    parse_one_declaration_into(
        &mut declarations,
        PropertyId::NonCustom(ShorthandId::Font.into()),
        font,
        Origin::Author,
        &url_data,
        None,
        ParsingMode::DEFAULT,
        document.quirks_mode(),
        CssRuleType::Style,
    )
    .map_err(|()| Error::Syntax(None))?;

    let mut families = Vec::new();
    let mut descriptor = FontDescriptor {
        weight: FontWeight::NORMAL,
        width: FontWidth::NORMAL,
        style: FontStyle::NORMAL,
        variant: FontVariantCaps::Normal,
        pt_size: Au(0),
        variation_settings: Vec::new(),
        synthesis_weight: FontSynthesis::Auto,
        optical_sizing: FontOpticalSizing::Auto,
    };
    // Values that only resolve at computed-value time (calc() with relative units) cannot be
    // absolutized against initial values here, so they are treated like unparsable input.
    for declaration in declarations.declarations.iter() {
        match declaration {
            // A CSS-wide keyword (or a var() reference) is a syntax error for this algorithm.
            PropertyDeclaration::CSSWideKeyword(..) | PropertyDeclaration::WithVariables(..) => {
                return Err(Error::Syntax(None));
            },
            PropertyDeclaration::FontFamily(family) => match family {
                SpecifiedFontFamily::Values(list) => families = list.iter().cloned().collect(),
                SpecifiedFontFamily::System(system) => match *system {},
            },
            PropertyDeclaration::FontWeight(weight) => {
                descriptor.weight = match weight {
                    SpecifiedFontWeight::Absolute(absolute) => {
                        absolute.compute().ok_or(Error::Syntax(None))?
                    },
                    SpecifiedFontWeight::Bolder => FontWeight::NORMAL.bolder(),
                    SpecifiedFontWeight::Lighter => FontWeight::NORMAL.lighter(),
                    SpecifiedFontWeight::System(system) => match *system {},
                }
            },
            PropertyDeclaration::FontStyle(font_style) => {
                descriptor.style = match font_style {
                    SpecifiedFontStyleProperty::Specified(GenericFontStyle::Italic) => {
                        FontStyle::ITALIC
                    },
                    SpecifiedFontStyleProperty::Specified(GenericFontStyle::Oblique(angle)) => {
                        FontStyle::oblique(
                            angle
                                .degrees()
                                .ok_or(Error::Syntax(None))?
                                .clamp(
                                    FONT_STYLE_OBLIQUE_MIN_ANGLE_DEGREES,
                                    FONT_STYLE_OBLIQUE_MAX_ANGLE_DEGREES,
                                ),
                        )
                    },
                    SpecifiedFontStyleProperty::System(system) => match *system {},
                }
            },
            PropertyDeclaration::FontWidth(width) => {
                descriptor.width = match width {
                    SpecifiedFontWidth::Keyword(keyword) => keyword.compute(),
                    SpecifiedFontWidth::Width(percentage) => FontWidth::from_percentage(
                        percentage.compute().ok_or(Error::Syntax(None))?.0,
                    ),
                    SpecifiedFontWidth::System(system) => match *system {},
                }
            },
            _ => {},
        }
    }
    Ok((families, descriptor))
}

/// Resolve `promise` with the result of waiting for all of `status_promises`, in order.
fn resolve_with_all_status_promises(
    cx: &mut CurrentRealm,
    global: &GlobalScope,
    promise: &Promise,
    status_promises: Vec<RootedPromise>,
) {
    let all = wait_for_all_promise(cx, global, status_promises);
    rooted!(&in(cx) let all_value = ObjectValue(all.promise_obj().get()));
    promise.resolve(cx, all_value.handle());
}

/// <https://drafts.csswg.org/css-font-loading/#FontFaceSet-interface>
#[dom_struct]
pub(crate) struct FontFaceSet {
    target: EventTarget,

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-readypromise-slot>
    promise: DomRefCell<TracedPromise>,

    set_entries: DomRefCell<Vec<Dom<FontFace>>>,
}

impl FontFaceSet {
    fn new_inherited(promise: &RootedPromise) -> Self {
        FontFaceSet {
            target: EventTarget::new_inherited(),
            promise: DomRefCell::new(promise.to_traced()),
            set_entries: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let promise = Promise::new(cx, global);
        reflect_dom_object_with_proto(
            cx,
            Box::new(FontFaceSet::new_inherited(&promise)),
            global,
            proto,
        )
    }

    pub(super) fn handle_font_face_status_changed(&self, cx: &mut JSContext, font_face: &FontFace) {
        match font_face.Status() {
            FontFaceLoadStatus::Loading => {
                self.switch_to_loading(cx);
            },
            FontFaceLoadStatus::Loaded => {
                let Some(window) = DomRoot::downcast::<Window>(self.global()) else {
                    return;
                };

                let (family_name, template) = font_face
                    .template()
                    .expect("A loaded web font should have a template");
                window
                    .font_context()
                    .add_template_to_font_context(family_name, template);
                window.Document().dirty_all_nodes(cx.no_gc());
            },
            _ => {},
        }
    }

    /// Fulfill the font ready promise, returning true if it was not already fulfilled beforehand.
    pub(crate) fn fulfill_ready_promise_if_needed(&self, cx: &mut JSContext) -> bool {
        let promise = self.promise.borrow();
        if promise.is_fulfilled() {
            return false;
        }
        promise.resolve_native(cx, self);
        true
    }

    pub(crate) fn waiting_to_fullfill_promise(&self) -> bool {
        !self.promise.borrow().is_fulfilled()
    }

    fn contains_face(&self, target: &FontFace) -> bool {
        self.set_entries
            .borrow()
            .iter()
            .any(|face| &**face == target)
    }

    /// Removes a face from the set's set entries.
    fn delete_face(&self, target: &FontFace) -> bool {
        let mut set_entries = self.set_entries.borrow_mut();
        let Some(index) = set_entries.iter().position(|face| &**face == target) else {
            return false;
        };
        set_entries.remove(index);
        true
    }

    /// <https://drafts.csswg.org/css-font-loading/#switch-the-fontfaceset-to-loading>
    pub(crate) fn switch_to_loading(&self, cx: &mut JSContext) {
        // Step 1. Let font face set be the given FontFaceSet.
        // Note: This is self.

        // Step 2. Set the status attribute of font face set to "loading".
        // TODO: Implement the FontFaceSet status attribute.

        // Step 3. If font face set’s [[ReadyPromise]] slot currently holds a fulfilled
        // promise, replace it with a fresh pending promise.
        if self.promise.borrow().is_fulfilled() {
            let promise = Promise::new(cx, &self.global());
            *self.promise.borrow_mut() = promise.to_traced()
        }

        // Step 4. Queue a task to fire a font load event named loading at font face set.
        // TODO: Implement support for font loading events.
    }

    /// Runs the CSS cascade to ensure that new `@font-face` rules have
    /// an entry in this set.
    fn flush_author_font_set(&self, cx: &mut JSContext) {
        // FIXME: Use a new sort of ReflowGoal that only runs the CSS cascade without
        //        building a new box tree or running any sort of layout really.
        //        We query for the box area here, but we're not interested in the result.
        // FIXME: Figure out what to do for worker scopes.
        if let Some(window) = DomRoot::downcast::<Window>(self.global()) {
            let document = window.Document();
            if document.stylesheets_changed_since_last_reflow() {
                window.reflow(cx, ReflowGoal::LayoutQuery(QueryMsg::BoxArea));
            }
        }
    }

    /// Marks the entries corresponding to removed `@font-face` rules as not [css-connected].
    ///
    /// [css-connected]: https://drafts.csswg.org/css-font-loading/#css-connected
    pub(crate) fn notify_font_face_rules_removed(
        &self,
        removed_font_face_rules: &[ServoArc<FontFaceRuleInfo>],
    ) {
        let entries = self.set_entries.borrow_mut();
        for removed_font_face_rule in removed_font_face_rules {
            let Some(matching_font_face_object) = entries
                .iter()
                .find(|entry| entry.is_connected_to_font_face_rule(removed_font_face_rule))
            else {
                if cfg!(debug_assertions) {
                    unreachable!("Removed @font-face that was not previously present");
                }
                log::warn!("Removed @font-face that was not previously present");
                continue;
            };

            // https://drafts.csswg.org/css-font-loading/#font-face-css-connection:
            // > If a @font-face rule is removed from the document, its corresponding FontFace object is no longer CSS-connected.
            // > The connection is not restorable by any means.
            matching_font_face_object.disconnect_from_css();
        }
    }

    /// Load the faces in this set that font matching selected during the last layout. Browsers
    /// load a `FontFace` in the document's font source on demand, when matching first needs it.
    pub(crate) fn load_faces_requested_by_font_matching(&self, cx: &mut JSContext) {
        let global = self.global();
        let font_context = global.as_window().font_context();
        if !font_context.has_unloaded_script_face_load_requests() {
            return;
        }
        let requested: Vec<DomRoot<FontFace>> = self
            .set_entries
            .borrow()
            .iter()
            .filter(|face| face.take_font_matching_load_request(font_context))
            .map(|face| face.as_rooted())
            .collect();
        for face in requested {
            let _ = face.Load(cx);
        }
    }

    /// <https://drafts.csswg.org/css-font-loading/#find-the-matching-font-faces>
    fn find_matching_font_faces(&self, font: &str, text: &str) -> Fallible<MatchingFontFaces> {
        // Step 1. Parse font using the CSS value syntax of the font property. If a syntax error
        // occurs, return a syntax error. If the parsed value is a CSS-wide keyword, return a
        // syntax error. Absolutize all relative lengths against the initial values of the
        // corresponding properties.
        // Step 3. Let font family list be the list of font families parsed from font, and font
        // style be the other font style attributes parsed from font.
        let global = self.global();
        let (families, font_style) = parse_font_for_matching(global.as_window(), font)?;

        // Step 2. If text was not explicitly provided, let it be a string containing a single
        // space character (U+0020 SPACE).
        // Note: the IDL default value of `text` provides this.

        // Step 4. Let available font faces be the available font faces within source.
        // Note: faces whose descriptors failed to parse have no family and never match.
        let available_faces: Vec<_> = self
            .set_entries
            .borrow()
            .iter()
            .filter_map(|face| {
                let css_descriptors = face.css_font_face_descriptors()?;
                let mut descriptor = FontTemplateDescriptor::default();
                descriptor.override_values_with_css_font_template_descriptors(&css_descriptors);
                Some((face.as_rooted(), css_descriptors.family_name, descriptor))
            })
            .collect();

        // Step 5. Let matched font faces initially be an empty list.
        let mut matched: Vec<(DomRoot<FontFace>, FontTemplateDescriptor)> = Vec::new();
        let mut family_names = Vec::new();

        // Step 6. For each family in font family list, use the font matching rules to select the
        // font faces from available font faces that match the font style, and add them to
        // matched font faces. The use of the unicode-range descriptor means that this may be more
        // than just a single font face.
        for family in families {
            // Generic families only ever match system fonts.
            let SingleFontFamily::FamilyName(family) = family else {
                continue;
            };
            let family_name: LowercaseFontFamilyName = family.name.clone().into();
            let family_faces = || {
                available_faces
                    .iter()
                    .filter(|(_, face_family_name, _)| *face_family_name == family_name)
            };
            let best_distance = family_faces()
                .map(|(_, _, descriptor)| descriptor.distance_from(&font_style))
                .fold(f32::MAX, f32::min);
            matched.extend(
                family_faces()
                    .filter(|(_, _, descriptor)| {
                        descriptor.distance_from(&font_style) == best_distance
                    })
                    .map(|(face, _, descriptor)| (face.clone(), descriptor.clone())),
            );
            family_names.push(family_name);
        }

        // Step 7. If matched font faces is empty, set the found faces flag to false. Otherwise,
        // set it to true.
        // Note: no caller uses the found faces flag.

        // Step 8. For each font face in matched font faces, if its defined unicode-range does not
        // include the codepoint of at least one character in text, remove it from the list.
        // Step 9. Return matched font faces and the found faces flag.
        Ok(MatchingFontFaces {
            font_faces: matched
                .into_iter()
                .filter(|(_, descriptor)| {
                    text.chars()
                        .any(|character| descriptor.char_in_unicode_range(character))
                })
                .map(|(face, _)| face)
                .collect(),
            family_names,
        })
    }
}

impl FontFaceSetMethods<crate::DomTypeHolder> for FontFaceSet {
    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-ready>
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        if self.promise.borrow().is_fulfilled() {
            // There may be pending style changes that cause new web fonts to start loading,
            // re-initializing document.fonts.ready.
            self.flush_author_font_set(cx);
        }
        self.promise.borrow().root(cx)
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-add>
    fn Add(&self, cx: &mut JSContext, font_face: &FontFace) -> Fallible<DomRoot<FontFaceSet>> {
        // Step 1. If font is already in the FontFaceSet’s set entries,
        // skip to the last step of this algorithm immediately.
        if self.contains_face(font_face) {
            return Ok(DomRoot::from_ref(self));
        }

        // Step 2. If font is CSS-connected, throw an InvalidModificationError
        // exception and exit this algorithm immediately.
        if font_face.is_css_connected() {
            return Err(Error::InvalidModification(Some(
                "Cannot add CSS-connected FontFace to FontFaceSet".to_owned(),
            )));
        }

        // Step 3. Add the font argument to the FontFaceSet’s set entries.
        self.set_entries.borrow_mut().push(Dom::from_ref(font_face));
        font_face.set_associated_font_face_set(self);
        if let Some(window) = DomRoot::downcast::<Window>(self.global()) {
            font_face.add_to_font_matching(cx, &window);
        }

        // Step 4. If font’s status attribute is "loading":
        // Step 4.1 If the FontFaceSet’s [[LoadingFonts]] list is empty, switch the FontFaceSet to loading.
        // Step 4.2 Append font to the FontFaceSet’s [[LoadingFonts]] list.
        self.handle_font_face_status_changed(cx, font_face);

        // Step 5. Return the FontFaceSet.
        Ok(DomRoot::from_ref(self))
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-delete>
    fn Delete(&self, to_delete: &FontFace) -> bool {
        // Step 1. If font is CSS-connected, return false and exit this algorithm immediately.
        if to_delete.is_css_connected() {
            return false;
        }

        // Step 2. Let deleted be the result of removing font from the FontFaceSet’s set entries.
        // TODO: Step 3. If font is present in the FontFaceSet’s [[LoadedFonts]], or [[FailedFonts]] lists, remove it.
        // TODO: Step 4. If font is present in the FontFaceSet’s [[LoadingFonts]] list, remove it. If font was the last
        // item in that list (and so the list is now empty), switch the FontFaceSet to loaded.
        // Step 5. Return deleted.
        let deleted = self.delete_face(to_delete);
        if deleted {
            to_delete.remove_from_font_matching();
        }
        deleted
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-clear>
    fn Clear(&self, cx: &mut JSContext) {
        self.flush_author_font_set(cx);

        // Step 1. Remove all non-CSS-connected items from the FontFaceSet’s set entries,
        // its [[LoadedFonts]] list, and its [[FailedFonts]] list.
        // Root every face before removing the set's traced references to them.
        let faces: Vec<_> = self
            .set_entries
            .borrow()
            .iter()
            .map(Dom::as_rooted)
            .collect();
        self.set_entries.borrow_mut().clear();
        for face in faces {
            face.remove_from_font_matching();
        }

        // TODO Step 2. If the FontFaceSet’s [[LoadingFonts]] list is non-empty, remove all items from it,
        // then switch the FontFaceSet to loaded.
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-load>
    fn Load(&self, cx: &mut JSContext, font: DOMString, text: DOMString) -> RootedPromise {
        // Step 1. Let font face set be the FontFaceSet object this method was called on. Let
        // promise be a newly-created promise object.
        let load_promise = Promise::new(cx, &self.global());

        /// Resolves the load promise once the stylesheet (CSS-connected) fonts are loaded.
        #[derive(MallocSizeOf, JSTraceable)]
        #[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
        struct StylesheetFontsLoadedHandler {
            load_promise: TracedPromise,
            status_promises: Vec<TracedPromise>,
        }
        impl Callback for StylesheetFontsLoadedHandler {
            fn callback(&self, cx: &mut CurrentRealm, _: Handle<Value>) {
                let global = self.load_promise.global();
                let status_promises = self
                    .status_promises
                    .iter()
                    .map(|promise| promise.root(cx))
                    .collect();
                resolve_with_all_status_promises(
                    cx,
                    &global,
                    &self.load_promise,
                    status_promises,
                );
            }
        }

        // Step 4. Queue a task to run the following steps synchronously:
        let trusted_this = Trusted::new(self);
        let trusted_load_promise = TrustedPromise::from(&load_promise);
        let font = font.to_string();
        let text = text.to_string();
        self.global()
            .task_manager()
            .font_loading_task_source()
            .queue(task!(resolve_font_face_set_load_task: move |cx| {
                let load_promise = trusted_load_promise.root(cx);
                let this = trusted_this.root();

                // This will need adjustments once FontFaceSet is exposed to workers.
                let Some(window) = DomRoot::downcast::<Window>(this.global()) else {
                    log::error!("FontFaceSet should not be exposed to non-window globals");
                    return;
                };

                // Step 3. Find the matching font faces from font face set using the font and text
                // arguments passed to the function, and let font face list be the return value (ignoring
                // the found faces flag). If a syntax error was returned, reject promise with a SyntaxError
                // exception and terminate these steps.
                let matching = match this.find_matching_font_faces(&font, &text) {
                    Ok(matching) => matching,
                    Err(error) => {
                        load_promise.reject_error(cx, error);
                        return;
                    },
                };

                // Step 4.1. For all of the font faces in the font face list, call their load()
                // method.
                //
                // CSS-connected faces are loaded by the stylesheet machinery and their
                // [[FontStatusPromise]] is not settled by it, so they are represented by a
                // promise resolved with the face, and the set's ready promise is waited on below.
                let global = this.global();
                let mut waits_for_stylesheet_fonts = false;
                let status_promises: Vec<RootedPromise> = matching
                    .font_faces
                    .iter()
                    .map(|face| {
                        if face.is_css_connected() {
                            waits_for_stylesheet_fonts = true;
                            Promise::new_resolved(cx, &global, face.clone())
                        } else {
                            face.Load(cx)
                        }
                    })
                    .collect();

                // Step 4.2. Resolve promise with the result of waiting for all of the
                // [[FontStatusPromise]]s of each font face in the font face list, in order.
                let font_context = window.font_context();
                waits_for_stylesheet_fonts |= matching
                    .family_names
                    .iter()
                    .any(|family_name| font_context.is_loading_stylesheet_web_font(family_name));
                if waits_for_stylesheet_fonts {
                    let handler = PromiseNativeHandler::new(
                        cx,
                        &global,
                        Some(Box::new(StylesheetFontsLoadedHandler {
                            load_promise: load_promise.to_traced(),
                            status_promises: status_promises
                                .iter()
                                .map(RootedPromise::to_traced)
                                .collect(),
                        })),
                        None,
                    );
                    let ready_promise = this.Ready(cx);
                    let mut realm = enter_auto_realm(cx, &*global);
                    ready_promise.append_native_handler(&mut realm.current_realm(), &handler);
                } else {
                    let mut realm = enter_auto_realm(cx, &*global);
                    resolve_with_all_status_promises(
                        &mut realm.current_realm(),
                        &global,
                        &load_promise,
                        status_promises,
                    );
                }
            }));

        // Step 2. Return promise. Complete the rest of these steps asynchronously.
        load_promise
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-check>
    fn Check(&self, font: DOMString, text: DOMString) -> Fallible<bool> {
        // Step 1. Let font face set be the FontFaceSet object this method was called on.
        // Step 2. Find the matching font faces from font face set using the font and text
        // arguments passed to the function, and including system fonts, and let font face list
        // be the returned list of font faces, and found faces be the returned found faces flag.
        // If a syntax error was returned, throw a SyntaxError exception and terminate these
        // steps.
        let matching = self.find_matching_font_faces(&font.str(), &text.str())?;

        // Step 3. If font face list is empty, or all fonts in the font face list either have a
        // status attribute of "loaded" or are system fonts, return true. Otherwise, return false.
        //
        // The status of CSS-connected faces is not updated when their `@font-face` rule loads;
        // one of the families still being fetched stands in for an unloaded face from such a
        // rule.
        let global = self.global();
        let font_context = global.as_window().font_context();
        Ok(matching
            .font_faces
            .iter()
            .all(|face| face.is_css_connected() || face.Status() == FontFaceLoadStatus::Loaded) &&
            !matching
                .family_names
                .iter()
                .any(|family_name| font_context.is_loading_stylesheet_web_font(family_name)))
    }

    /// <https://html.spec.whatwg.org/multipage/#customstateset>
    fn Size(&self, cx: &mut JSContext) -> u32 {
        self.size(cx)
    }
}

impl Setlike for FontFaceSet {
    type Key = DomRoot<FontFace>;

    #[inline(always)]
    fn get_index(&self, cx: &mut JSContext, index: u32) -> Option<Self::Key> {
        self.flush_author_font_set(cx);
        self.set_entries
            .borrow()
            .get(index as usize)
            .map(|face| face.as_rooted())
    }

    #[inline(always)]
    fn size(&self, cx: &mut JSContext) -> u32 {
        self.flush_author_font_set(cx);
        self.set_entries.borrow().len() as u32
    }

    #[inline(always)]
    fn add(&self, _cx: &mut JSContext, face: Self::Key) {
        self.set_entries.borrow_mut().push(face.as_traced());
    }

    #[inline(always)]
    fn has(&self, cx: &mut JSContext, target: Self::Key) -> bool {
        self.flush_author_font_set(cx);
        self.contains_face(&target)
    }

    #[inline(always)]
    fn clear(&self, cx: &mut JSContext) {
        self.flush_author_font_set(cx);
        self.set_entries.borrow_mut().clear();
    }

    #[inline(always)]
    fn delete(&self, cx: &mut JSContext, to_delete: Self::Key) -> bool {
        self.flush_author_font_set(cx);
        self.delete_face(&to_delete)
    }
}
