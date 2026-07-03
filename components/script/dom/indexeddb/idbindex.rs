/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::gc::MutableHandleValue;
use js::rust::HandleValue;
use script_bindings::codegen::GenericBindings::IDBIndexBinding::IDBIndexMethods;
use script_bindings::conversions::SafeToJSValConvertible;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use script_bindings::str::DOMString;
use storage_traits::indexeddb::{AsyncOperation, AsyncReadOnlyOperation};

use crate::dom::bindings::codegen::Bindings::IDBCursorBinding::IDBCursorDirection;
use crate::dom::bindings::error::Fallible;
use js::context::JSContext as SafeJSContext;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::globalscope::GlobalScope;
use crate::dom::idbobjectstore::KeyPath;
use crate::dom::indexeddb::idbcursor::{IDBCursor, IterationParam, ObjectStoreOrIndex};
use crate::dom::indexeddb::idbcursorwithvalue::IDBCursorWithValue;
use crate::dom::indexeddb::idbobjectstore::IDBObjectStore;
use crate::dom::indexeddb::idbrequest::IDBRequest;
use crate::indexeddb::convert_value_to_key_range;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct IDBIndex {
    reflector_: Reflector,
    object_store: DomRoot<IDBObjectStore>,
    name: DOMString,
    multi_entry: bool,
    unique: bool,
    key_path: KeyPath,
}

impl IDBIndex {
    pub fn new_inherited(
        object_store: DomRoot<IDBObjectStore>,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
    ) -> IDBIndex {
        IDBIndex {
            reflector_: Reflector::new(),
            object_store,
            name,
            multi_entry,
            unique,
            key_path,
        }
    }

    pub fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        object_store: DomRoot<IDBObjectStore>,
        name: DOMString,
        multi_entry: bool,
        unique: bool,
        key_path: KeyPath,
    ) -> DomRoot<IDBIndex> {
        reflect_dom_object_with_cx(
            Box::new(IDBIndex::new_inherited(
                object_store,
                name,
                multi_entry,
                unique,
                key_path,
            )),
            global,
            cx,
        )
    }

    /// The index's name (its identifier within its object store).
    pub(crate) fn name(&self) -> &DOMString {
        &self.name
    }

    /// The index's key path, used to extract the index key from a stored value.
    pub(crate) fn key_path(&self) -> &KeyPath {
        &self.key_path
    }

    /// Whether this is a `multiEntry` index (an array index key yields one entry per element).
    pub(crate) fn is_multi_entry(&self) -> bool {
        self.multi_entry
    }

    /// The object store this index belongs to. Used as the request source / for transaction
    /// access when running index queries (the index reads route through the store's transaction).
    pub(crate) fn object_store(&self) -> &IDBObjectStore {
        &self.object_store
    }

    /// Shared open-cursor logic for [`OpenCursor`](Self::OpenCursor) /
    /// [`OpenKeyCursor`](Self::OpenKeyCursor). Mirrors the object-store variant, but the cursor's
    /// source is this index (so its key is the index key and its primary key is the record key),
    /// and the iterate operation carries this index's name so the backend walks `index_data`. (LYK-1310)
    fn open_cursor(
        &self,
        cx: &mut js::context::JSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
        key_only: bool,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;

        let range = convert_value_to_key_range(cx, query, Some(false))?;
        let transaction = self.object_store().transaction();

        let cursor = if key_only {
            IDBCursor::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            )
        } else {
            DomRoot::upcast(IDBCursorWithValue::new(
                cx,
                &self.global(),
                &transaction,
                direction,
                false,
                ObjectStoreOrIndex::Index(Dom::from_ref(self)),
                range.clone(),
                key_only,
            ))
        };

        let iteration_param = IterationParam {
            cursor: Trusted::new(&cursor),
            key: None,
            primary_key: None,
            count: None,
        };
        let index_name = self.name.to_string();
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Iterate {
                    callback,
                    key_range: range,
                    index: Some(index_name),
                })
            },
            None,
            Some(iteration_param),
        )
        .inspect(|request| cursor.set_request(request))
    }
}

impl IDBIndexMethods<crate::DomTypeHolder> for IDBIndex {
    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-objectstore>
    fn ObjectStore(&self) -> DomRoot<IDBObjectStore> {
        self.object_store.clone()
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-multientry>
    fn MultiEntry(&self) -> bool {
        self.multi_entry
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-unique>
    fn Unique(&self) -> bool {
        self.unique
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-keypath>
    fn KeyPath(&self, cx: &mut JSContext, retval: MutableHandleValue) {
        match &self.key_path {
            KeyPath::String(string) => {
                string.safe_to_jsval(cx, retval);
            },
            KeyPath::StringSequence(sequence) => {
                sequence.safe_to_jsval(cx, retval);
            },
        }
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-get>
    fn Get(&self, cx: &mut SafeJSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;
        let index_name = self.name.to_string();
        let range = convert_value_to_key_range(cx, query, Some(true))?;
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetItem {
                    callback,
                    key_range: range,
                    index: Some(index_name),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-getkey>
    fn GetKey(&self, cx: &mut SafeJSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;
        let index_name = self.name.to_string();
        let range = convert_value_to_key_range(cx, query, Some(true))?;
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetKey {
                    callback,
                    key_range: range,
                    index: Some(index_name),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-getall>
    fn GetAll(
        &self,
        cx: &mut SafeJSContext,
        query: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;
        let index_name = self.name.to_string();
        let range = convert_value_to_key_range(cx, query, None)?;
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetAllItems {
                    callback,
                    key_range: range,
                    count,
                    index: Some(index_name),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-getallkeys>
    fn GetAllKeys(
        &self,
        cx: &mut SafeJSContext,
        query: HandleValue,
        count: Option<u32>,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;
        let index_name = self.name.to_string();
        let range = convert_value_to_key_range(cx, query, None)?;
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::GetAllKeys {
                    callback,
                    key_range: range,
                    count,
                    index: Some(index_name),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-count>
    fn Count(&self, cx: &mut SafeJSContext, query: HandleValue) -> Fallible<DomRoot<IDBRequest>> {
        self.object_store().verify_not_deleted()?;
        self.object_store().check_transaction_active()?;
        let index_name = self.name.to_string();
        let range = convert_value_to_key_range(cx, query, None)?;
        IDBRequest::execute_async(
            cx,
            self.object_store(),
            |callback| {
                AsyncOperation::ReadOnly(AsyncReadOnlyOperation::Count {
                    callback,
                    key_range: range,
                    index: Some(index_name),
                })
            },
            None,
            None,
        )
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-opencursor>
    fn OpenCursor(
        &self,
        cx: &mut SafeJSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, false)
    }

    /// <https://www.w3.org/TR/IndexedDB/#dom-idbindex-openkeycursor>
    fn OpenKeyCursor(
        &self,
        cx: &mut SafeJSContext,
        query: HandleValue,
        direction: IDBCursorDirection,
    ) -> Fallible<DomRoot<IDBRequest>> {
        self.open_cursor(cx, query, direction, true)
    }
}
