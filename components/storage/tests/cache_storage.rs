/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::{Path, PathBuf};

use servo_base::generic_channel::{self, GenericCallback};
use servo_base::id::{PIPELINE_NAMESPACE, PipelineNamespace, PipelineNamespaceId};
use servo_url::{ImmutableOrigin, ServoUrl};
use storage::cache_storage::{query_cache, request_matches_cached_item};
use storage::{CacheStorageThreadFactory, ClientStorageThreadFactory};
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheApiResponseType, CacheBatchError,
    CacheMatchScope, CacheStorageThreadHandle, CacheStorageThreadMessage,
    CacheStorageThreadResponse,
};
use storage_traits::client_storage::{
    ClientStorageThreadHandle, StorageIdentifier, StorageProxyMap, StorageType,
};

fn request(url: &str) -> CacheApiRequest {
    CacheApiRequest {
        url: url.to_owned(),
        method: "GET".to_owned(),
        headers: vec![],
    }
}

fn response(body: &str) -> CacheApiResponse {
    CacheApiResponse {
        status: 200,
        status_message: b"OK".to_vec(),
        headers: vec![],
        url: None,
        response_type: CacheApiResponseType::Basic,
        body: body.as_bytes().to_vec(),
    }
}

#[test]
fn test_exit() {
    let handle: CacheStorageThreadHandle = CacheStorageThreadFactory::new(None, false);

    let (sender, receiver) = generic_channel::channel().unwrap();
    handle
        .send(CacheStorageThreadMessage::Exit(sender.into()))
        .unwrap();
    receiver.recv().unwrap();

    // Workaround for https://github.com/servo/servo/issues/32912
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(1000));
}

#[test]
fn test_request_matching() {
    let options = CacheApiQueryOptions::default();
    let cached = request("https://example.com/a?x=1");

    // Fragments are always excluded.
    assert!(request_matches_cached_item(
        &request("https://example.com/a?x=1#frag"),
        &cached,
        None,
        &options
    ));
    // The query is significant unless ignoreSearch is set.
    let other_query = request("https://example.com/a?x=2");
    assert!(!request_matches_cached_item(
        &other_query,
        &cached,
        None,
        &options
    ));
    let ignore_search = CacheApiQueryOptions {
        ignore_search: true,
        ..Default::default()
    };
    assert!(request_matches_cached_item(
        &other_query,
        &cached,
        None,
        &ignore_search
    ));

    // Non-GET queries only match with ignoreMethod.
    let post = CacheApiRequest {
        method: "POST".to_owned(),
        ..cached.clone()
    };
    assert!(!request_matches_cached_item(&post, &cached, None, &options));
    let ignore_method = CacheApiQueryOptions {
        ignore_method: true,
        ..Default::default()
    };
    assert!(request_matches_cached_item(
        &post,
        &cached,
        None,
        &ignore_method
    ));
}

#[test]
fn test_vary_matching() {
    let cached = CacheApiRequest {
        headers: vec![("Accept-Language".to_owned(), b"en".to_vec())],
        ..request("https://example.com/")
    };
    let mut varying = response("en");
    varying.headers = vec![("Vary".to_owned(), b"accept-language".to_vec())];
    let storage = vec![(cached.clone(), varying.clone())];

    let same = CacheApiRequest {
        headers: vec![("accept-language".to_owned(), b"en".to_vec())],
        ..request("https://example.com/")
    };
    let different = CacheApiRequest {
        headers: vec![("accept-language".to_owned(), b"fr".to_vec())],
        ..request("https://example.com/")
    };
    let options = CacheApiQueryOptions::default();
    assert_eq!(query_cache(&same, &options, &storage), vec![0]);
    assert!(query_cache(&different, &options, &storage).is_empty());
    let ignore_vary = CacheApiQueryOptions {
        ignore_vary: true,
        ..Default::default()
    };
    assert_eq!(query_cache(&different, &ignore_vary, &storage), vec![0]);

    // `Vary: *` never matches.
    let mut star = varying;
    star.headers = vec![("Vary".to_owned(), b"*".to_vec())];
    assert!(query_cache(&same, &options, &[(cached, star)]).is_empty());
}

/// Send a message built around a blocking callback and wait for the reply.
fn roundtrip(
    handle: &CacheStorageThreadHandle,
    message: impl FnOnce(GenericCallback<CacheStorageThreadResponse>) -> CacheStorageThreadMessage,
) -> CacheStorageThreadResponse {
    let (callback, receiver) = GenericCallback::new_blocking().unwrap();
    handle.send(message(callback)).unwrap();
    receiver.recv().unwrap()
}

/// A client storage thread rooted in `dir`, and a storage bottle for `origin`'s caches.
fn client_storage_proxy(dir: &Path, origin: &ImmutableOrigin) -> StorageProxyMap {
    if PIPELINE_NAMESPACE.get().is_none() {
        PipelineNamespace::install(PipelineNamespaceId(1));
    }
    let client_storage: ClientStorageThreadHandle =
        ClientStorageThreadFactory::new(Some(dir.to_path_buf()), false);
    client_storage
        .obtain_a_storage_bottle_map(
            StorageType::Local,
            None,
            StorageIdentifier::Caches,
            origin.clone(),
        )
        .recv()
        .unwrap()
        .unwrap()
}

fn exit(handle: &CacheStorageThreadHandle) {
    let (sender, receiver) = generic_channel::channel().unwrap();
    handle
        .send(CacheStorageThreadMessage::Exit(sender.into()))
        .unwrap();
    receiver.recv().unwrap();
}

/// With a config dir, the cache storage is SQLite-backed.
#[test]
fn test_cache_workflow_sqlite() {
    let tmp_dir = tempfile::tempdir().unwrap();
    cache_workflow(tmp_dir.path(), Some(tmp_dir.path().to_path_buf()), false);
}

/// Without a config dir, or with temporary storage, the cache storage is in memory.
#[test]
fn test_cache_workflow_memory() {
    let tmp_dir = tempfile::tempdir().unwrap();
    cache_workflow(tmp_dir.path(), None, false);
    let tmp_dir = tempfile::tempdir().unwrap();
    cache_workflow(tmp_dir.path(), Some(tmp_dir.path().to_path_buf()), true);
    assert!(!tmp_dir.path().join("cache_api.sqlite").exists());
}

/// Caches written by one cache storage thread are read back by the next one using the same
/// config dir.
#[test]
fn test_cache_persistence() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let origin: ImmutableOrigin = ServoUrl::parse("https://example.com").unwrap().origin();
    let proxy = client_storage_proxy(tmp_dir.path(), &origin);

    let handle: CacheStorageThreadHandle =
        CacheStorageThreadFactory::new(Some(tmp_dir.path().to_path_buf()), false);
    for name in ["v1", "v2"] {
        let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::OpenCache {
            cache_name: name.to_owned(),
            callback,
            proxy: proxy.clone(),
            origin: origin.clone(),
        });
        assert!(matches!(
            reply,
            CacheStorageThreadResponse::OpenCacheResult { result: Ok(()), .. }
        ));
    }
    let mut stored = response("persisted");
    stored.headers = vec![("Content-Type".to_owned(), b"text/plain".to_vec())];
    stored.url = Some("https://example.com/a".to_owned());
    stored.response_type = CacheApiResponseType::Cors;
    let mut stored_request = request("https://example.com/a");
    stored_request.headers = vec![("Accept".to_owned(), b"*/*".to_vec())];
    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Put {
        cache_name: "v2".to_owned(),
        entries: vec![(stored_request.clone(), stored.clone())],
        callback,
        origin: origin.clone(),
    });
    assert!(matches!(
        reply,
        CacheStorageThreadResponse::PutResult(Ok(()))
    ));
    exit(&handle);
    assert!(tmp_dir.path().join("cache_api.sqlite").exists());

    // Reopen the same config dir.
    let handle: CacheStorageThreadHandle =
        CacheStorageThreadFactory::new(Some(tmp_dir.path().to_path_buf()), false);
    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::CacheNames {
        callback,
        origin: origin.clone(),
    });
    let CacheStorageThreadResponse::CacheNamesResult(Ok(names)) = reply else {
        panic!("unexpected reply {reply:?}");
    };
    assert_eq!(names, vec!["v1".to_owned(), "v2".to_owned()]);

    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Match {
        scope: CacheMatchScope::AllCaches,
        request: Some(request("https://example.com/a")),
        options: CacheApiQueryOptions::default(),
        max_results: usize::MAX,
        callback,
        origin: origin.clone(),
    });
    let CacheStorageThreadResponse::MatchResult(Ok(responses)) = reply else {
        panic!("unexpected reply {reply:?}");
    };
    assert_eq!(responses, vec![stored]);

    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Keys {
        cache_name: "v2".to_owned(),
        request: None,
        options: CacheApiQueryOptions::default(),
        callback,
        origin: origin.clone(),
    });
    let CacheStorageThreadResponse::KeysResult(Ok(keys)) = reply else {
        panic!("unexpected reply {reply:?}");
    };
    assert_eq!(keys, vec![stored_request]);

    // Other origins don't see the caches.
    let other: ImmutableOrigin = ServoUrl::parse("https://other.example").unwrap().origin();
    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::CacheNames {
        callback,
        origin: other,
    });
    assert!(matches!(
        reply,
        CacheStorageThreadResponse::CacheNamesResult(Ok(names)) if names.is_empty()
    ));
    exit(&handle);
}

fn cache_workflow(client_dir: &Path, cache_dir: Option<PathBuf>, temporary_storage: bool) {
    let origin: ImmutableOrigin = ServoUrl::parse("https://example.com").unwrap().origin();
    let proxy = client_storage_proxy(client_dir, &origin);
    let handle: CacheStorageThreadHandle =
        CacheStorageThreadFactory::new(cache_dir, temporary_storage);

    for name in ["v1", "v2"] {
        let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::OpenCache {
            cache_name: name.to_owned(),
            callback,
            proxy: proxy.clone(),
            origin: origin.clone(),
        });
        assert!(matches!(
            reply,
            CacheStorageThreadResponse::OpenCacheResult { result: Ok(()), .. }
        ));
    }

    // CacheStorage.keys reports caches in creation order.
    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::CacheNames {
        callback,
        origin: origin.clone(),
    });
    let CacheStorageThreadResponse::CacheNamesResult(Ok(names)) = reply else {
        panic!("unexpected reply {reply:?}");
    };
    assert_eq!(names, vec!["v1".to_owned(), "v2".to_owned()]);

    let put = |cache_name: &str, entries: Vec<(CacheApiRequest, CacheApiResponse)>| {
        let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Put {
            cache_name: cache_name.to_owned(),
            entries,
            callback,
            origin: origin.clone(),
        });
        let CacheStorageThreadResponse::PutResult(result) = reply else {
            panic!("unexpected reply {reply:?}");
        };
        result
    };
    let match_all = |scope: CacheMatchScope, url: Option<&str>, max_results: usize| {
        let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Match {
            scope,
            request: url.map(request),
            options: CacheApiQueryOptions::default(),
            max_results,
            callback,
            origin: origin.clone(),
        });
        let CacheStorageThreadResponse::MatchResult(Ok(responses)) = reply else {
            panic!("unexpected reply {reply:?}");
        };
        responses
            .into_iter()
            .map(|response| String::from_utf8(response.body).unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        put(
            "v1",
            vec![
                (request("https://example.com/a"), response("a1")),
                (request("https://example.com/b"), response("b1")),
            ]
        ),
        Ok(())
    );
    assert_eq!(
        put(
            "v2",
            vec![(request("https://example.com/a"), response("a2"))]
        ),
        Ok(())
    );
    // A put replaces the entry its request matches.
    assert_eq!(
        put(
            "v1",
            vec![(request("https://example.com/b#x"), response("b2"))]
        ),
        Ok(())
    );
    // A batch that targets the same request twice fails as a whole.
    assert_eq!(
        put(
            "v1",
            vec![
                (request("https://example.com/c"), response("c1")),
                (request("https://example.com/c"), response("c2")),
            ]
        ),
        Err(CacheBatchError::DuplicateRequest)
    );
    assert_eq!(
        put(
            "missing",
            vec![(request("https://example.com/a"), response("a"))]
        ),
        Err(CacheBatchError::NoSuchCache)
    );

    let v1 = || CacheMatchScope::Cache("v1".to_owned());
    assert_eq!(match_all(v1(), None, usize::MAX), vec!["a1", "b2"]);
    assert_eq!(
        match_all(v1(), Some("https://example.com/c"), usize::MAX),
        Vec::<String>::new()
    );
    // CacheStorage.match searches caches in creation order and returns the first match.
    assert_eq!(
        match_all(CacheMatchScope::AllCaches, Some("https://example.com/a"), 1),
        vec!["a1"]
    );
    assert_eq!(
        match_all(
            CacheMatchScope::AllCaches,
            Some("https://example.com/a"),
            usize::MAX
        ),
        vec!["a1", "a2"]
    );

    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::Keys {
        cache_name: "v1".to_owned(),
        request: None,
        options: CacheApiQueryOptions::default(),
        callback,
        origin: origin.clone(),
    });
    let CacheStorageThreadResponse::KeysResult(Ok(keys)) = reply else {
        panic!("unexpected reply {reply:?}");
    };
    let urls: Vec<_> = keys.into_iter().map(|request| request.url).collect();
    assert_eq!(
        urls,
        vec!["https://example.com/a", "https://example.com/b#x"]
    );

    let reply = roundtrip(&handle, |callback| {
        CacheStorageThreadMessage::DeleteEntries {
            cache_name: "v1".to_owned(),
            request: request("https://example.com/a"),
            options: CacheApiQueryOptions::default(),
            callback,
            origin: origin.clone(),
        }
    });
    assert!(matches!(
        reply,
        CacheStorageThreadResponse::DeleteEntriesResult(Ok(true))
    ));
    assert_eq!(match_all(v1(), None, usize::MAX), vec!["b2"]);

    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::DeleteCache {
        cache_name: "v1".to_owned(),
        callback,
        proxy: proxy.clone(),
        origin: origin.clone(),
    });
    assert!(matches!(
        reply,
        CacheStorageThreadResponse::DeleteCacheResult(Ok(true))
    ));
    let reply = roundtrip(&handle, |callback| CacheStorageThreadMessage::HasCache {
        cache_name: "v1".to_owned(),
        callback,
        proxy: proxy.clone(),
        origin: origin.clone(),
    });
    assert!(matches!(
        reply,
        CacheStorageThreadResponse::HasCacheResult(Ok(false))
    ));

    exit(&handle);
}
