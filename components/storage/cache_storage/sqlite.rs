/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A SQLite-backed [`CacheStorageEngine`], so that the Cache API survives restarts.
//!
//! Every operation is written through immediately (each `put`/`delete` is durable on return; there
//! is no exit-time flush to lose on a crash). All origins share one database file, which keeps the
//! schema and file location of swervo's pre-merge Cache API storage, so profiles written by it
//! keep their caches.
//!
//! Matching runs in Rust with the same Query Cache code as the in-memory engine: a cache's entries
//! are loaded and filtered rather than queried in SQL. Per-cache entry counts are small (tens to
//! low hundreds for real PWAs) and the matching rules are much clearer as code than as SQL.

use std::path::Path;

use log::{info, warn};
use rusqlite::{Connection, OptionalExtension, params};
use servo_url::ImmutableOrigin;
use storage_traits::cache_storage::{
    CacheApiQueryOptions, CacheApiRequest, CacheApiResponse, CacheApiResponseType, CacheBatchError,
    CacheMatchScope, CacheStorageError,
};
use storage_traits::client_storage::StorageProxyMap;

use super::{CacheStorageEngine, query_cache};
use crate::shared::DB_PRAGMAS;

/// The database file, directly in the config dir (where pre-merge swervo kept it).
pub(super) const DB_FILE_NAME: &str = "cache_api.sqlite";

const SCHEMA: &str = "
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
        body BLOB NOT NULL,
        response_type INTEGER NOT NULL DEFAULT 2
    );
    CREATE INDEX IF NOT EXISTS entries_by_cache ON entries(cache_id);";

type Error = CacheStorageError<String>;

fn internal(error: impl ToString) -> Error {
    CacheStorageError::Internal(error.to_string())
}

fn response_type_to_sql(response_type: CacheApiResponseType) -> i64 {
    match response_type {
        CacheApiResponseType::Basic => 0,
        CacheApiResponseType::Cors => 1,
        CacheApiResponseType::Default => 2,
        CacheApiResponseType::Error => 3,
        CacheApiResponseType::Opaque => 4,
        CacheApiResponseType::OpaqueRedirect => 5,
    }
}

fn response_type_from_sql(value: i64) -> CacheApiResponseType {
    match value {
        0 => CacheApiResponseType::Basic,
        1 => CacheApiResponseType::Cors,
        3 => CacheApiResponseType::Error,
        4 => CacheApiResponseType::Opaque,
        5 => CacheApiResponseType::OpaqueRedirect,
        _ => CacheApiResponseType::Default,
    }
}

/// A stored entry: its row id and the request/response pair.
struct Entry {
    rowid: i64,
    request: CacheApiRequest,
    response: CacheApiResponse,
}

pub struct SqliteCacheStorageEngine {
    connection: Connection,
}

impl SqliteCacheStorageEngine {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let connection = Connection::open(path)?;
        info!("Opening Cache API storage at {:?}", path);
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(connection)
    }

    fn init(connection: Connection) -> rusqlite::Result<Self> {
        for stmt in DB_PRAGMAS {
            let _ = connection.execute_batch(stmt);
        }
        connection.execute_batch(SCHEMA)?;
        // Databases written before response types were stored lack the column.
        let has_response_type: bool = connection.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('entries') WHERE name = 'response_type'",
            [],
            |row| row.get(0),
        )?;
        if !has_response_type {
            connection.execute_batch(
                "ALTER TABLE entries ADD COLUMN response_type INTEGER NOT NULL DEFAULT 2;",
            )?;
        }
        Ok(SqliteCacheStorageEngine { connection })
    }

    fn cache_id(&self, origin: &ImmutableOrigin, cache_name: &str) -> Result<Option<i64>, Error> {
        self.connection
            .query_row(
                "SELECT id FROM caches WHERE origin = ?1 AND name = ?2",
                params![origin.ascii_serialization(), cache_name],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)
    }

    /// The ids of the origin's caches, in creation order.
    fn cache_ids(&self, origin: &ImmutableOrigin) -> Result<Vec<i64>, Error> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT id FROM caches WHERE origin = ?1 ORDER BY id")
            .map_err(internal)?;
        stmt.query_map(params![origin.ascii_serialization()], |row| row.get(0))
            .map_err(internal)?
            .collect::<rusqlite::Result<_>>()
            .map_err(internal)
    }

    /// A cache's entries, in insertion order.
    fn load_entries(&self, cache_id: i64) -> Result<Vec<Entry>, Error> {
        let mut stmt = self
            .connection
            .prepare_cached(
                "SELECT id, url, method, request_headers, status, status_message, \
                 response_headers, response_url, body, response_type \
                 FROM entries WHERE cache_id = ?1 ORDER BY id",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![cache_id], |row| {
                let request_headers: Vec<u8> = row.get(3)?;
                let response_headers: Vec<u8> = row.get(6)?;
                Ok(Entry {
                    rowid: row.get(0)?,
                    request: CacheApiRequest {
                        url: row.get(1)?,
                        method: row.get(2)?,
                        headers: postcard::from_bytes(&request_headers).unwrap_or_default(),
                    },
                    response: CacheApiResponse {
                        status: row.get::<_, i64>(4)? as u16,
                        status_message: row.get(5)?,
                        headers: postcard::from_bytes(&response_headers).unwrap_or_default(),
                        url: row.get(7)?,
                        body: row.get(8)?,
                        response_type: response_type_from_sql(row.get(9)?),
                    },
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<_>>().map_err(internal)
    }

    fn load_pairs(&self, cache_id: i64) -> Result<Vec<(CacheApiRequest, CacheApiResponse)>, Error> {
        Ok(self
            .load_entries(cache_id)?
            .into_iter()
            .map(|entry| (entry.request, entry.response))
            .collect())
    }
}

fn insert_entry(
    connection: &Connection,
    cache_id: i64,
    request: &CacheApiRequest,
    response: &CacheApiResponse,
) -> Result<(), String> {
    let request_headers = postcard::to_allocvec(&request.headers).map_err(|e| e.to_string())?;
    let response_headers = postcard::to_allocvec(&response.headers).map_err(|e| e.to_string())?;
    // Kept for the schema shared with pre-merge swervo; matching reads the response headers.
    let vary = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("vary"))
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned());
    connection
        .execute(
            "INSERT INTO entries(cache_id, url, method, request_headers, vary, status, \
             status_message, response_headers, response_url, body, response_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                response_type_to_sql(response.response_type),
            ],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

impl CacheStorageEngine for SqliteCacheStorageEngine {
    type Error = String;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    fn has_cache(&mut self, origin: ImmutableOrigin, cache_name: &str) -> Result<bool, Error> {
        Ok(self.cache_id(&origin, cache_name)?.is_some())
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    fn open_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: String,
        proxy_map: &StorageProxyMap,
    ) -> Result<(), Error> {
        // Step 2.1: If cacheName matches a key of the relevant name to cache map, resolve with
        // it and abort these steps.
        if self.cache_id(&origin, &cache_name)?.is_some() {
            return Ok(());
        }

        // Step 2.2 - 2.3: Let cache be a new request response list, and set the relevant name to
        // cache map[cacheName] to cache. As in the in-memory engine, the cache is also
        // registered with client storage.
        match proxy_map
            .handle
            .create_database(proxy_map.bottle_id, cache_name.clone())
            .recv()
        {
            Ok(Ok(_)) => {},
            Ok(Err(error)) => return Err(internal(error)),
            Err(error) => return Err(internal(format!("{error:?}"))),
        }
        self.connection
            .execute(
                "INSERT INTO caches(origin, name) VALUES (?1, ?2)",
                params![origin.ascii_serialization(), cache_name],
            )
            .map_err(internal)?;
        Ok(())
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-keys>
    fn cache_names(&mut self, origin: &ImmutableOrigin) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT name FROM caches WHERE origin = ?1 ORDER BY id")
            .map_err(internal)?;
        stmt.query_map(params![origin.ascii_serialization()], |row| row.get(0))
            .map_err(internal)?
            .collect::<rusqlite::Result<_>>()
            .map_err(internal)
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    fn keys(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
    ) -> Result<Vec<CacheApiRequest>, Error> {
        let Some(cache_id) = self.cache_id(origin, cache_name)? else {
            return Ok(Vec::new());
        };
        let entries = self.load_pairs(cache_id)?;
        Ok(match request {
            None => entries.into_iter().map(|(request, _)| request).collect(),
            Some(request) => query_cache(request, options, &entries)
                .into_iter()
                .map(|index| entries[index].0.clone())
                .collect(),
        })
    }

    /// <https://w3c.github.io/ServiceWorker/#cache-matchall>, also serving `Cache.match` and
    /// `CacheStorage.match`.
    fn match_all(
        &mut self,
        origin: &ImmutableOrigin,
        scope: &CacheMatchScope,
        request: Option<&CacheApiRequest>,
        options: &CacheApiQueryOptions,
        max_results: usize,
    ) -> Result<Vec<CacheApiResponse>, Error> {
        let cache_ids = match scope {
            CacheMatchScope::Cache(cache_name) => {
                self.cache_id(origin, cache_name)?.into_iter().collect()
            },
            CacheMatchScope::AllCaches => self.cache_ids(origin)?,
        };
        let mut responses = Vec::new();
        for cache_id in cache_ids {
            let entries = self.load_pairs(cache_id)?;
            match request {
                None => responses.extend(entries.into_iter().map(|(_, response)| response)),
                Some(request) => responses.extend(
                    query_cache(request, options, &entries)
                        .into_iter()
                        .map(|index| entries[index].1.clone()),
                ),
            }
            if responses.len() >= max_results {
                responses.truncate(max_results);
                break;
            }
        }
        Ok(responses)
    }

    /// <https://w3c.github.io/ServiceWorker/#batch-cache-operations>, for "put" operations.
    fn put(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        entries: Vec<(CacheApiRequest, CacheApiResponse)>,
    ) -> Result<(), CacheBatchError> {
        let to_batch_error = |error: Error| {
            let CacheStorageError::Internal(error) = error;
            CacheBatchError::Internal(error)
        };
        let Some(cache_id) = self.cache_id(origin, cache_name).map_err(to_batch_error)? else {
            return Err(CacheBatchError::NoSuchCache);
        };

        // Step 2: Let backupCache be a copy of cache. The operations run against a working copy
        // in Rust, and the resulting changes are applied in one transaction, so a failing batch
        // leaves the stored cache untouched.
        let mut working: Vec<(Option<i64>, CacheApiRequest, CacheApiResponse)> = self
            .load_entries(cache_id)
            .map_err(to_batch_error)?
            .into_iter()
            .map(|entry| (Some(entry.rowid), entry.request, entry.response))
            .collect();
        let mut removed_rows = Vec::new();
        let mut added_items: Vec<(CacheApiRequest, CacheApiResponse)> = Vec::new();
        let options = CacheApiQueryOptions::default();

        for (request, response) in entries {
            // Step 4.2.3: A request already added by this batch is an InvalidStateError.
            if !query_cache(&request, &options, &added_items).is_empty() {
                return Err(CacheBatchError::DuplicateRequest);
            }

            // Step 4.2.4 - 4.2.6.5: Replace the entries the request matches.
            let pairs: Vec<_> = working
                .iter()
                .map(|(_, request, response)| (request.clone(), response.clone()))
                .collect();
            let mut matched = query_cache(&request, &options, &pairs);
            matched.sort_unstable();
            for index in matched.into_iter().rev() {
                if let (Some(rowid), _, _) = working.remove(index) {
                    removed_rows.push(rowid);
                }
            }
            working.push((None, request.clone(), response.clone()));

            // Step 4.2.6.7: Append operation’s request and response to addedItems.
            added_items.push((request, response));
        }

        let transaction = self
            .connection
            .transaction()
            .map_err(|e| CacheBatchError::Internal(e.to_string()))?;
        for rowid in removed_rows {
            transaction
                .execute("DELETE FROM entries WHERE id = ?1", params![rowid])
                .map_err(|e| CacheBatchError::Internal(e.to_string()))?;
        }
        for (request, response) in &added_items {
            insert_entry(&transaction, cache_id, request, response).map_err(|error| {
                warn!("Cache API put failed: {error}");
                CacheBatchError::Internal(error)
            })?;
        }
        transaction
            .commit()
            .map_err(|e| CacheBatchError::Internal(e.to_string()))
    }

    /// <https://w3c.github.io/ServiceWorker/#batch-cache-operations>, for a "delete" operation.
    fn delete_entries(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        request: &CacheApiRequest,
        options: &CacheApiQueryOptions,
    ) -> Result<bool, Error> {
        let Some(cache_id) = self.cache_id(origin, cache_name)? else {
            return Ok(false);
        };
        let entries = self.load_entries(cache_id)?;
        let pairs: Vec<_> = entries
            .iter()
            .map(|entry| (entry.request.clone(), entry.response.clone()))
            .collect();
        let matched = query_cache(request, options, &pairs);
        if matched.is_empty() {
            return Ok(false);
        }
        let transaction = self.connection.transaction().map_err(internal)?;
        for index in matched {
            transaction
                .execute(
                    "DELETE FROM entries WHERE id = ?1",
                    params![entries[index].rowid],
                )
                .map_err(internal)?;
        }
        transaction.commit().map_err(internal)?;
        Ok(true)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-cachestorage-delete>
    fn delete_cache(
        &mut self,
        origin: ImmutableOrigin,
        cache_name: &str,
        proxy: &StorageProxyMap,
    ) -> Result<bool, Error> {
        let Some(cache_id) = self.cache_id(&origin, cache_name)? else {
            return Ok(false);
        };
        let transaction = self.connection.transaction().map_err(internal)?;
        transaction
            .execute("DELETE FROM entries WHERE cache_id = ?1", params![cache_id])
            .map_err(internal)?;
        transaction
            .execute("DELETE FROM caches WHERE id = ?1", params![cache_id])
            .map_err(internal)?;
        transaction.commit().map_err(internal)?;

        // Caches stored by pre-merge swervo were never registered with client storage, so a
        // failure to unregister is not an error.
        match proxy
            .handle
            .delete_database(proxy.bottle_id, cache_name.to_string())
            .recv()
        {
            Ok(Ok(())) => {},
            Ok(Err(error)) => warn!("Failed to unregister deleted cache {cache_name}: {error}"),
            Err(error) => warn!("Failed to unregister deleted cache {cache_name}: {error:?}"),
        }
        Ok(true)
    }
}
