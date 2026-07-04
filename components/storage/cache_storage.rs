/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Cache API storage thread (<https://w3c.github.io/ServiceWorker/#cache-objects>).
//!
//! Origin-keyed named caches of request → response pairs, SQLite-backed with write-through
//! persistence (each `put`/`delete` is durable immediately — no exit-time flush to lose on a
//! crash). One thread per storage group; the private (incognito) group uses an in-memory
//! database so nothing touches disk.
//!
//! Matching implements the spec's "query cache" algorithm: fragment-stripped URL comparison,
//! `ignoreSearch`/`ignoreMethod`/`ignoreVary`, and `Vary` header comparison against the stored
//! request headers. Entries for a cache are loaded and filtered in Rust rather than in SQL —
//! per-cache entry counts are small (tens to low hundreds for real PWAs) and the matching rules
//! are much clearer as code than as SQL.

use std::path::PathBuf;
use std::thread;

use log::{error, warn};
use rusqlite::{Connection, params};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use servo_url::ImmutableOrigin;
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheStorageThreadMsg,
};

pub trait CacheStorageThreadFactory {
    fn new(config_dir: Option<PathBuf>, in_memory: bool) -> Self;
}

impl CacheStorageThreadFactory for GenericSender<CacheStorageThreadMsg> {
    /// Spawn the cache storage thread for one storage group.
    fn new(
        config_dir: Option<PathBuf>,
        in_memory: bool,
    ) -> GenericSender<CacheStorageThreadMsg> {
        let (chan, port) = generic_channel::channel().unwrap();
        thread::Builder::new()
            .name("CacheStorageManager".to_owned())
            .spawn(move || match CacheStorageManager::new(port, config_dir, in_memory) {
                Ok(manager) => manager.start(),
                Err(e) => error!("Cache API storage failed to initialize: {e}"),
            })
            .expect("Thread spawning failed");
        chan
    }
}

struct CacheStorageManager {
    port: GenericReceiver<CacheStorageThreadMsg>,
    db: Connection,
}

/// A stored entry, hydrated for matching.
struct Entry {
    rowid: i64,
    url: String,
    method: String,
    request_headers: Vec<(String, Vec<u8>)>,
    vary: Option<String>,
    status: u16,
    status_message: Vec<u8>,
    response_headers: Vec<(String, Vec<u8>)>,
    response_url: Option<String>,
    body: Vec<u8>,
}

/// Strip the fragment (and optionally the query) for URL comparison. Operates textually so
/// unparseable stored URLs still compare stably against themselves.
fn comparison_url(url: &str, ignore_search: bool) -> &str {
    let end_frag = url.find('#').unwrap_or(url.len());
    let no_frag = &url[..end_frag];
    if ignore_search {
        let end_query = no_frag.find('?').unwrap_or(no_frag.len());
        &no_frag[..end_query]
    } else {
        no_frag
    }
}

fn header_value<'h>(headers: &'h [(String, Vec<u8>)], name: &str) -> Option<&'h [u8]> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_slice())
}

/// <https://w3c.github.io/ServiceWorker/#request-matches-cached-item>
fn request_matches(
    query: &CacheApiRequest,
    entry: &Entry,
    options: &CacheApiQueryOptions,
) -> bool {
    // Step: unless ignoreMethod, only GET/HEAD queries can match (stored entries are GETs).
    if !options.ignore_method &&
        !query.method.eq_ignore_ascii_case("GET") &&
        !query.method.eq_ignore_ascii_case("HEAD")
    {
        return false;
    }

    if comparison_url(&query.url, options.ignore_search) !=
        comparison_url(&entry.url, options.ignore_search)
    {
        return false;
    }

    // Vary: every listed request-header must have the same value now as when stored; `*` never
    // matches.
    if !options.ignore_vary {
        if let Some(vary) = &entry.vary {
            for field in vary.split(',') {
                let field = field.trim();
                if field.is_empty() {
                    continue;
                }
                if field == "*" {
                    return false;
                }
                if header_value(&query.headers, field) !=
                    header_value(&entry.request_headers, field)
                {
                    return false;
                }
            }
        }
    }

    true
}

impl CacheStorageManager {
    fn new(
        port: GenericReceiver<CacheStorageThreadMsg>,
        config_dir: Option<PathBuf>,
        in_memory: bool,
    ) -> Result<Self, String> {
        let db = match (&config_dir, in_memory) {
            (Some(dir), false) => {
                let _ = std::fs::create_dir_all(dir);
                Connection::open(dir.join("cache_api.sqlite")).map_err(|e| e.to_string())?
            },
            // Private group / no profile dir: keep everything in memory.
            _ => Connection::open_in_memory().map_err(|e| e.to_string())?,
        };
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS caches(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 origin TEXT NOT NULL,
                 name TEXT NOT NULL,
                 UNIQUE(origin, name)
             );
             CREATE TABLE IF NOT EXISTS entries(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 cache_id INTEGER NOT NULL REFERENCES caches(id) ON DELETE CASCADE,
                 url TEXT NOT NULL,
                 method TEXT NOT NULL,
                 request_headers BLOB NOT NULL,
                 vary TEXT,
                 status INTEGER NOT NULL,
                 status_message BLOB NOT NULL,
                 response_headers BLOB NOT NULL,
                 response_url TEXT,
                 body BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS entries_by_cache ON entries(cache_id);",
        )
        .map_err(|e| e.to_string())?;
        Ok(CacheStorageManager { port, db })
    }

    fn start(self) {
        loop {
            let Ok(msg) = self.port.recv() else {
                // All senders gone; shut down with the process.
                break;
            };
            match msg {
                CacheStorageThreadMsg::Open {
                    sender,
                    origin,
                    name,
                } => {
                    let _ = sender.send(self.open(&origin, &name));
                },
                CacheStorageThreadMsg::Has {
                    sender,
                    origin,
                    name,
                } => {
                    let _ = sender.send(self.cache_id(&origin, &name).is_some());
                },
                CacheStorageThreadMsg::Delete {
                    sender,
                    origin,
                    name,
                } => {
                    let _ = sender.send(self.delete_cache(&origin, &name));
                },
                CacheStorageThreadMsg::Names { sender, origin } => {
                    let _ = sender.send(self.names(&origin));
                },
                CacheStorageThreadMsg::Match {
                    sender,
                    origin,
                    cache_id,
                    cache_name,
                    request,
                    options,
                    max_results,
                } => {
                    let _ = sender.send(self.query(
                        &origin,
                        cache_id,
                        cache_name.as_deref(),
                        request.as_ref(),
                        &options,
                        max_results,
                    ));
                },
                CacheStorageThreadMsg::Put {
                    sender,
                    cache_id,
                    request,
                    response,
                } => {
                    let _ = sender.send(self.put(cache_id, &request, &response));
                },
                CacheStorageThreadMsg::DeleteEntry {
                    sender,
                    cache_id,
                    request,
                    options,
                } => {
                    let _ = sender.send(self.delete_entry(cache_id, &request, &options));
                },
                CacheStorageThreadMsg::Keys {
                    sender,
                    cache_id,
                    request,
                    options,
                } => {
                    let _ = sender.send(self.keys(cache_id, request.as_ref(), &options));
                },
                CacheStorageThreadMsg::Exit(sender) => {
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }

    fn cache_id(&self, origin: &ImmutableOrigin, name: &str) -> Option<i64> {
        self.db
            .query_row(
                "SELECT id FROM caches WHERE origin = ?1 AND name = ?2",
                params![origin.ascii_serialization(), name],
                |row| row.get(0),
            )
            .ok()
    }

    fn open(&self, origin: &ImmutableOrigin, name: &str) -> Result<i64, String> {
        if let Some(id) = self.cache_id(origin, name) {
            return Ok(id);
        }
        self.db
            .execute(
                "INSERT INTO caches(origin, name) VALUES (?1, ?2)",
                params![origin.ascii_serialization(), name],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.db.last_insert_rowid())
    }

    fn delete_cache(&self, origin: &ImmutableOrigin, name: &str) -> bool {
        let Some(id) = self.cache_id(origin, name) else {
            return false;
        };
        // Manual cascade: foreign_keys pragma is connection-scoped and cheap to not rely on.
        let _ = self
            .db
            .execute("DELETE FROM entries WHERE cache_id = ?1", params![id]);
        self.db
            .execute("DELETE FROM caches WHERE id = ?1", params![id])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    fn names(&self, origin: &ImmutableOrigin) -> Vec<String> {
        let Ok(mut stmt) = self
            .db
            .prepare("SELECT name FROM caches WHERE origin = ?1 ORDER BY id")
        else {
            return vec![];
        };
        stmt.query_map(params![origin.ascii_serialization()], |row| row.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    fn load_entries(&self, cache_id: i64) -> Vec<Entry> {
        let Ok(mut stmt) = self.db.prepare(
            "SELECT id, url, method, request_headers, vary, status, status_message, \
             response_headers, response_url, body FROM entries WHERE cache_id = ?1 ORDER BY id",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map(params![cache_id], |row| {
            let request_headers: Vec<u8> = row.get(3)?;
            let response_headers: Vec<u8> = row.get(7)?;
            Ok(Entry {
                rowid: row.get(0)?,
                url: row.get(1)?,
                method: row.get(2)?,
                request_headers: postcard::from_bytes(&request_headers).unwrap_or_default(),
                vary: row.get(4)?,
                status: row.get::<_, i64>(5)? as u16,
                status_message: row.get(6)?,
                response_headers: postcard::from_bytes(&response_headers).unwrap_or_default(),
                response_url: row.get(8)?,
                body: row.get(9)?,
            })
        });
        rows.map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Cache ids to search: a specific cache, a named cache, or all of the origin's caches in
    /// creation order (`CacheStorage.match`).
    fn caches_to_search(
        &self,
        origin: &ImmutableOrigin,
        cache_id: Option<i64>,
        cache_name: Option<&str>,
    ) -> Vec<i64> {
        if let Some(id) = cache_id {
            return vec![id];
        }
        if let Some(name) = cache_name {
            return self.cache_id(origin, name).into_iter().collect();
        }
        let Ok(mut stmt) = self
            .db
            .prepare("SELECT id FROM caches WHERE origin = ?1 ORDER BY id")
        else {
            return vec![];
        };
        stmt.query_map(params![origin.ascii_serialization()], |row| row.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    fn query(
        &self,
        origin: &ImmutableOrigin,
        cache_id: Option<i64>,
        cache_name: Option<&str>,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
        max_results: u32,
    ) -> Vec<CacheApiResponse> {
        let mut out = Vec::new();
        for id in self.caches_to_search(origin, cache_id, cache_name) {
            for entry in self.load_entries(id) {
                let matches = match request {
                    Some(request) => request_matches(request, &entry, options),
                    None => true,
                };
                if matches {
                    out.push(CacheApiResponse {
                        status: entry.status,
                        status_message: entry.status_message,
                        headers: entry.response_headers,
                        url: entry.response_url,
                        body: entry.body,
                    });
                    if out.len() as u32 >= max_results {
                        return out;
                    }
                }
            }
        }
        out
    }

    fn put(
        &self,
        cache_id: i64,
        request: &CacheApiRequest,
        response: &CacheApiResponse,
    ) -> Result<(), String> {
        // Batch cache operations replace any entry the incoming request matches.
        for entry in self.load_entries(cache_id) {
            if request_matches(request, &entry, &CacheApiQueryOptions::default()) {
                let _ = self
                    .db
                    .execute("DELETE FROM entries WHERE id = ?1", params![entry.rowid]);
            }
        }
        let request_headers =
            postcard::to_allocvec(&request.headers).map_err(|e| e.to_string())?;
        let response_headers =
            postcard::to_allocvec(&response.headers).map_err(|e| e.to_string())?;
        let vary = header_value(&response.headers, "vary")
            .map(|v| String::from_utf8_lossy(v).into_owned());
        self.db
            .execute(
                "INSERT INTO entries(cache_id, url, method, request_headers, vary, status, \
                 status_message, response_headers, response_url, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    cache_id,
                    request.url,
                    request.method,
                    request_headers,
                    vary,
                    response.status as i64,
                    response.status_message,
                    response_headers,
                    response.url,
                    response.body,
                ],
            )
            .map_err(|e| {
                warn!("Cache API put failed: {e}");
                e.to_string()
            })?;
        Ok(())
    }

    fn delete_entry(
        &self,
        cache_id: i64,
        request: &CacheApiRequest,
        options: &CacheApiQueryOptions,
    ) -> bool {
        let mut deleted = false;
        for entry in self.load_entries(cache_id) {
            if request_matches(request, &entry, options) {
                deleted |= self
                    .db
                    .execute("DELETE FROM entries WHERE id = ?1", params![entry.rowid])
                    .map(|n| n > 0)
                    .unwrap_or(false);
            }
        }
        deleted
    }

    fn keys(
        &self,
        cache_id: i64,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
    ) -> Vec<CacheApiRequest> {
        self.load_entries(cache_id)
            .into_iter()
            .filter(|entry| match request {
                Some(request) => request_matches(request, entry, options),
                None => true,
            })
            .map(|entry| CacheApiRequest {
                url: entry.url,
                method: entry.method,
                headers: entry.request_headers,
            })
            .collect()
    }
}
