/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::rc::Rc;

use app_units::Au;
use dom_struct::dom_struct;
use fonts::{FontDescriptor, FontTemplateDescriptor, LowercaseFontFamilyName};
use js::context::JSContext;
use js::gc::Handle;
use js::jsapi::Value;
use js::jsval::ObjectValue;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::FontFaceBinding::{
    FontFaceLoadStatus, FontFaceMethods,
};
use script_bindings::like::Setlike;
use script_bindings::reflector::reflect_dom_object_with_proto_and_cx;
use style::computed_values::font_optical_sizing::T as FontOpticalSizing;
use style::computed_values::font_variant_caps::T as FontVariantCaps;
use style::properties::{
    PropertyDeclaration, PropertyId, ShorthandId, SourcePropertyDeclaration,
    parse_one_declaration_into,
};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::computed::font::SingleFontFamily;
use style::values::computed::{FontStretch, FontStyle, FontSynthesis, FontWeight};
use style::values::generics::font::FontStyle as GenericFontStyle;
use style::values::specified::font::{
    FontFamily as SpecifiedFontFamily, FontStretch as SpecifiedFontStretch,
    FontStyle as SpecifiedFontStyleProperty, FontWeight as SpecifiedFontWeight,
    SpecifiedFontStyle,
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
use crate::dom::promise::{Promise, wait_for_all_promise};
use crate::dom::promisenativehandler::Callback;
use crate::dom::types::PromiseNativeHandler;
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;

/// The result of <https://drafts.csswg.org/css-font-loading/#find-the-matching-font-faces>.
struct MatchingFontFaces {
    font_faces: Vec<DomRoot<FontFace>>,
    /// The named families of the `font` argument. `@font-face` rules have no [`FontFace`] objects
    /// in this implementation, so callers consult the font context about these families.
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
        stretch: FontStretch::NORMAL,
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
                            SpecifiedFontStyle::compute_angle_degrees(angle)
                                .ok_or(Error::Syntax(None))?,
                        )
                    },
                    SpecifiedFontStyleProperty::System(system) => match *system {},
                }
            },
            PropertyDeclaration::FontStretch(stretch) => {
                descriptor.stretch = match stretch {
                    SpecifiedFontStretch::Keyword(keyword) => keyword.compute(),
                    SpecifiedFontStretch::Stretch(percentage) => FontStretch::from_percentage(
                        percentage.compute().ok_or(Error::Syntax(None))?.0,
                    ),
                    SpecifiedFontStretch::System(system) => match *system {},
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
    status_promises: Vec<Rc<Promise>>,
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
    #[conditional_malloc_size_of]
    promise: RefCell<Rc<Promise>>,

    set_entries: DomRefCell<Vec<Dom<FontFace>>>,
}

impl FontFaceSet {
    fn new_inherited(cx: &mut JSContext, global: &GlobalScope) -> Self {
        FontFaceSet {
            target: EventTarget::new_inherited(),
            promise: Promise::new(cx, global).into(),
            set_entries: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto_and_cx(
            Box::new(FontFaceSet::new_inherited(cx, global)),
            global,
            proto,
            cx,
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
                window.Document().dirty_all_nodes();
            },
            _ => {},
        }
    }

    /// Fulfill the font ready promise, returning true if it was not already fulfilled beforehand.
    pub(crate) fn fulfill_ready_promise_if_needed(&self, cx: &mut JSContext) -> bool {
        let promise = self.promise.borrow().clone();
        if promise.is_fulfilled() {
            return false;
        }
        promise.resolve_native(cx, self);
        true
    }

    pub(crate) fn waiting_to_fullfill_promise(&self) -> bool {
        !self.promise.borrow().is_fulfilled()
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
            face.Load(cx);
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
            *self.promise.borrow_mut() = Promise::new(cx, &self.global());
        }

        // Step 4. Queue a task to fire a font load event named loading at font face set.
        // TODO: Implement support for font loading events.
    }
}

impl FontFaceSetMethods<crate::DomTypeHolder> for FontFaceSet {
    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-ready>
    fn Ready(&self) -> Rc<Promise> {
        self.promise.borrow().clone()
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-add>
    fn Add(&self, cx: &mut JSContext, font_face: &FontFace) -> DomRoot<FontFaceSet> {
        // Step 1. If font is already in the FontFaceSet’s set entries,
        // skip to the last step of this algorithm immediately.
        if self.contains_face(font_face) {
            return DomRoot::from_ref(self);
        }

        // TODO: Step 2. If font is CSS-connected, throw an InvalidModificationError
        // exception and exit this algorithm immediately.

        // Step 3. Add the font argument to the FontFaceSet’s set entries.
        self.set_entries.borrow_mut().push(Dom::from_ref(font_face));
        font_face.set_associated_font_face_set(self);
        if let Some(window) = DomRoot::downcast::<Window>(self.global()) {
            font_face.add_to_font_matching(&window);
        }

        // Step 4. If font’s status attribute is "loading":
        // Step 4.1 If the FontFaceSet’s [[LoadingFonts]] list is empty, switch the FontFaceSet to loading.
        // Step 4.2 Append font to the FontFaceSet’s [[LoadingFonts]] list.
        self.handle_font_face_status_changed(cx, font_face);

        // Step 5. Return the FontFaceSet.
        DomRoot::from_ref(self)
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-delete>
    fn Delete(&self, to_delete: &FontFace) -> bool {
        // TODO Step 1. If font is CSS-connected, return false and exit this algorithm immediately.

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
    fn Clear(&self) {
        // Step 1. Remove all non-CSS-connected items from the FontFaceSet’s set entries,
        // its [[LoadedFonts]] list, and its [[FailedFonts]] list.
        for face in self.set_entries.borrow_mut().drain(..) {
            face.remove_from_font_matching();
        }

        // TODO Step 2. If the FontFaceSet’s [[LoadingFonts]] list is non-empty, remove all items from it,
        // then switch the FontFaceSet to loaded.
    }

    /// <https://drafts.csswg.org/css-font-loading/#dom-fontfaceset-load>
    fn Load(&self, cx: &mut JSContext, font: DOMString, text: DOMString) -> Rc<Promise> {
        // Step 1. Let font face set be the FontFaceSet object this method was called on. Let
        // promise be a newly-created promise object.
        let load_promise = Promise::new(cx, &self.global());

        // Step 3. Find the matching font faces from font face set using the font and text
        // arguments passed to the function, and let font face list be the return value (ignoring
        // the found faces flag). If a syntax error was returned, reject promise with a SyntaxError
        // exception and terminate these steps.
        let matching = match self.find_matching_font_faces(&font.str(), &text.str()) {
            Ok(matching) => matching,
            Err(error) => {
                load_promise.reject_error(cx, error);
                return load_promise;
            },
        };
        let font_faces: Vec<Trusted<FontFace>> = matching
            .font_faces
            .iter()
            .map(|face| Trusted::new(&**face))
            .collect();
        let family_names = matching.family_names;

        // `@font-face` rules have no FontFace objects here, so their loads cannot be waited on
        // individually. They are all fetched as soon as their stylesheet is added, so wait for
        // the set to finish loading instead.
        #[derive(MallocSizeOf, JSTraceable)]
        struct StylesheetFontsLoadedHandler {
            #[conditional_malloc_size_of]
            load_promise: Rc<Promise>,
            #[conditional_malloc_size_of]
            status_promises: Vec<Rc<Promise>>,
        }
        impl Callback for StylesheetFontsLoadedHandler {
            fn callback(&self, cx: &mut CurrentRealm, _: Handle<Value>) {
                let global = self.load_promise.global();
                resolve_with_all_status_promises(
                    cx,
                    &global,
                    &self.load_promise,
                    self.status_promises.clone(),
                );
            }
        }

        // Step 4. Queue a task to run the following steps synchronously:
        let trusted_ready_promise = TrustedPromise::new(self.promise.borrow().clone());
        let trusted_load_promise = TrustedPromise::new(load_promise.clone());
        self.global()
            .task_manager()
            .font_loading_task_source()
            .queue(task!(resolve_font_face_set_load_task: move |cx| {
                let ready_promise = trusted_ready_promise.root();
                let load_promise = trusted_load_promise.root();
                let global = load_promise.global();

                // Step 4.1. For all of the font faces in the font face list, call their load()
                // method.
                let status_promises: Vec<Rc<Promise>> = font_faces
                    .iter()
                    .map(|face| face.root().Load(cx))
                    .collect();

                // Step 4.2. Resolve promise with the result of waiting for all of the
                // [[FontStatusPromise]]s of each font face in the font face list, in order.
                let font_context = global.as_window().font_context();
                let stylesheet_fonts_loading = family_names
                    .iter()
                    .any(|family_name| font_context.is_loading_stylesheet_web_font(family_name));
                let mut realm = enter_auto_realm(cx, &*global);
                let cx = &mut realm.current_realm();
                if stylesheet_fonts_loading {
                    let handler = PromiseNativeHandler::new(
                        cx,
                        &global,
                        Some(Box::new(StylesheetFontsLoadedHandler {
                            load_promise,
                            status_promises,
                        })),
                        None,
                    );
                    ready_promise.append_native_handler(cx, &handler);
                } else {
                    resolve_with_all_status_promises(cx, &global, &load_promise, status_promises);
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
        // `@font-face` rules have no FontFace objects here; one of the families still being
        // fetched stands in for an unloaded face from such a rule.
        let global = self.global();
        let font_context = global.as_window().font_context();
        Ok(matching
            .font_faces
            .iter()
            .all(|face| face.Status() == FontFaceLoadStatus::Loaded) &&
            !matching
                .family_names
                .iter()
                .any(|family_name| font_context.is_loading_stylesheet_web_font(family_name)))
    }

    /// <https://html.spec.whatwg.org/multipage/#customstateset>
    fn Size(&self) -> u32 {
        self.set_entries.borrow().len() as u32
    }
}

impl Setlike for FontFaceSet {
    type Key = DomRoot<FontFace>;

    #[inline(always)]
    fn get_index(&self, index: u32) -> Option<Self::Key> {
        self.set_entries
            .borrow()
            .get(index as usize)
            .map(|face| face.as_rooted())
    }

    #[inline(always)]
    fn size(&self) -> u32 {
        self.set_entries.borrow().len() as u32
    }

    #[inline(always)]
    fn add(&self, face: Self::Key) {
        self.set_entries.borrow_mut().push(face.as_traced());
    }

    #[inline(always)]
    fn has(&self, target: Self::Key) -> bool {
        self.contains_face(&target)
    }

    #[inline(always)]
    fn clear(&self) {
        self.set_entries.borrow_mut().clear();
    }

    #[inline(always)]
    fn delete(&self, to_delete: Self::Key) -> bool {
        self.delete_face(&to_delete)
    }
}
