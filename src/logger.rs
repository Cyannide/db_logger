// db_logger
// Copyright 2022 Julio Merino
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License.  You may obtain a copy
// of the License at:
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.  See the
// License for the specific language governing permissions and limitations
// under the License.

//! Database-backed logger implementation.
//!
//! The code in this module tries to be resilient to errors that it might itself cause.  To that
//! end, errors are logged and ignored.  And because we cannot rely on the logging facilities to be
//! functional (and because using them would cause us to recurse), any errors are printed to
//! `stderr`.

use crate::clocks::{Clock, SystemClock};
use crate::{Connection, Db, Result};
use gethostname::gethostname;
use log::{Level, Log, Metadata, Record};
use std::env;
use std::str::FromStr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

/// Maximum number of log calls we can ingest without blocking.
///
/// Ingesting a log entry into the `recorder` is a CPU-bound operation that does not involve any
/// I/O so a small size should be sufficient.
const CHANNEL_SIZE: usize = 128;

/// Maximum number of log entries to batch in each database write.
const MAX_BATCH_SIZE: usize = 128;

/// Maximum delay between log flushes.
const MAX_FLUSH_DELAY_SECS: u64 = 5;

/// Default log level when `RUST_LOG` is not set.
const DEFAULT_LOG_LEVEL: Level = Level::Warn;

// Maximum sizes of the corresponding fields in the schema.
// TODO(jmmv): We should not impose the restrictions of one backend (postgres) on others (sqlite).
pub(crate) const LOG_ENTRY_MAX_HOSTNAME_LENGTH: usize = 64;
pub(crate) const LOG_ENTRY_MAX_MODULE_LENGTH: usize = 64;
pub(crate) const LOG_ENTRY_MAX_FILENAME_LENGTH: usize = 256;
pub(crate) const LOG_ENTRY_MAX_MESSAGE_LENGTH: usize = 4096;

/// Contents of a log entry.
#[derive(Debug)]
pub(crate) struct LogEntry {
    pub(crate) timestamp: OffsetDateTime,
    pub(crate) hostname: String,
    pub(crate) level: Level,
    pub(crate) module: Option<String>,
    pub(crate) filename: Option<String>,
    pub(crate) line: Option<u32>,
    pub(crate) message: String,
}

#[derive(Debug)]
/// Types of requests that can be sent to the `recorder` background task.
enum Action {
    /// Asks the recorder to stop immediately.
    Stop,

    /// Asks the recorder to flush any pending messages and waits for completion.
    Flush,

    /// Asks the recorder to persist the provided log entry.
    Record(LogEntry),
}

/// Writes all `entries` to the `db` in a single transaction.
async fn write_all(db: Arc<dyn Db + Send + Sync + 'static>, entries: Vec<LogEntry>) {
    if let Err(e) = db.put_log_entries(entries).await {
        eprintln!("Failed to write log entries: {}", e);
    }
}

/// Background task that persists log entries to the database.
///
/// This task consumes log requests from the `action_rx` channel.  If any of these requests is a
/// flush or stop, then the requester can wait for completion by waiting on the `done_rx` channel.
///
/// Errors that occur here are dumped to stderr as we cannot do anything else about them.
///
/// Any log messages triggered by this routine must be filtered out at the logger level or else we
/// may enter an infinite loop.
async fn recorder(
    db: Arc<dyn Db + Send + Sync + 'static>,
    action_rx: mpsc::Receiver<Action>,
    done_tx: mpsc::SyncSender<()>,
) {
    let mut buffer = vec![];
    let mut writers = vec![];

    let timeout = Duration::new(MAX_FLUSH_DELAY_SECS, 0);
    loop {
        let auto_flush;
        let action = match action_rx.recv_timeout(timeout) {
            Ok(action) => {
                auto_flush = false;
                action
            }
            Err(RecvTimeoutError::Timeout) => {
                auto_flush = true;
                Action::Flush
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("Failed to get log entry due to closed channel; terminating logger");
                break;
            }
        };

        match action {
            Action::Stop => break,

            Action::Flush => {
                if !buffer.is_empty() {
                    let batch = buffer.split_off(0);
                    let db = db.clone();
                    writers.push(tokio::spawn(async move { write_all(db, batch).await }));
                }
                assert!(buffer.is_empty());

                for writer in writers.split_off(0) {
                    if let Err(e) = writer.await {
                        eprintln!("Failed to write batched entries: {}", e);
                    }
                }
                assert!(writers.is_empty());

                if !auto_flush {
                    done_tx.send(()).unwrap();
                }
            }

            Action::Record(entry) => {
                buffer.push(entry);

                if buffer.len() == MAX_BATCH_SIZE {
                    let batch = buffer.split_off(0);
                    let db = db.clone();
                    // TODO(jmmv): Should probably have some protection here and above to prevent
                    // the number of writers from growing unboundedly.
                    writers.push(tokio::spawn(async move { write_all(db, batch).await }));
                    assert!(buffer.is_empty());
                }
            }
        }
    }

    drop(db);
    done_tx.send(()).unwrap();
}

/// Returns true if `record` was potentially emitted by the code in `recorder`, which would cause us
/// to enter an infinite loop if not filtered out.
fn is_recorder_log(record: &Record) -> bool {
    // TODO(jmmv): Instead of blacklisting these modules, we should try to use tokio::task_local
    // to avoid log statements triggered by us.
    let module = match record.module_path() {
        Some(module) => module,
        None => return true,
    };
    (module.starts_with("rustls::") || module.starts_with("sqlx::"))
        || (record.level() >= Level::Trace
            && (module.starts_with("async_io::")
                || module.starts_with("async_std::")
                || module.starts_with("polling")))
}

/// Fetches the value of `RUST_LOG` or returns a default value if not available.
fn env_rust_log() -> Level {
    match env::var("RUST_LOG") {
        Ok(level) => match Level::from_str(&level) {
            Ok(level) => level,
            Err(e) => {
                eprintln!("Invalid RUST_LOG value: {}", e);
                DEFAULT_LOG_LEVEL
            }
        },
        Err(env::VarError::NotPresent) => DEFAULT_LOG_LEVEL,
        Err(e) => {
            eprintln!("Invalid RUST_LOG value: {}", e);
            DEFAULT_LOG_LEVEL
        }
    }
}

/// An opaque handler to maintain the logger's backing task alive.
///
/// Once this object goes out of scope, the logger's database persisting logic stops and attempts
/// to log may fail or get stuck.
// TODO(jmmv): Modify integration tests to check what happens and possibly refactor this to *not*
// expose this type at all.
pub struct Handle {
    db: Connection,
    action_tx: mpsc::SyncSender<Action>,
    done_rx: Arc<Mutex<mpsc::Receiver<()>>>,
}

impl Handle {
    /// Returns the sorted list of all log entries in the database.
    ///
    /// Given that this is exposed for testing purposes only, this just returns a flat textual
    /// representation of the log entry and does not try to deserialize it as a `LogEntry`.  This
    /// is for simplicity given that a `LogEntry` keeps references to static strings and we cannot
    /// obtain those from the database.
    pub async fn get_log_entries(&self) -> Result<Vec<String>> {
        self.db.0.get_log_entries().await
    }

    /// Flushes pending records to the backend DB
    pub fn flush(&self) {
        let done_rx = self.done_rx.lock().unwrap();
        self.action_tx.send(Action::Flush).unwrap();
        done_rx.recv().unwrap();
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let done_rx = self.done_rx.lock().unwrap();
        self.action_tx.send(Action::Flush).unwrap();
        done_rx.recv().unwrap();
        self.action_tx.send(Action::Stop).unwrap();
        done_rx.recv().unwrap();
    }
}

/// Implementation of a database-backed logger.
///
/// There should only be one instance of this object, which is persisted in a global `Box` owned by
/// the `log` crate.  As a result, this object gets never dropped.
struct DbLogger {
    hostname: String,
    action_tx: mpsc::SyncSender<Action>,
    done_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    clock: Arc<dyn Clock + Send + Sync + 'static>,
}

impl DbLogger {
    /// Creates a new logger backed by `db` that obtains timestamps from `clock` and that sets the
    /// hostname of the entries to `hostname`.
    async fn new(
        hostname: String,
        db: Connection,
        clock: Arc<dyn Clock + Send + Sync + 'static>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::sync_channel(CHANNEL_SIZE);
        let (done_tx, done_rx) = mpsc::sync_channel(1);

        tokio::spawn(async move {
            recorder(db.0, action_rx, done_tx).await;
        });

        let done_rx = Arc::from(Mutex::from(done_rx));
        Self { hostname, action_tx, done_rx, clock }
    }
}

impl Log for DbLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = self.clock.now_utc();

        // Skip logs emitted by the database-persistence code as they would cause us to recurse and
        // never finish logging.
        if is_recorder_log(record) {
            if record.level() <= Level::Warn {
                eprintln!(
                    "Non-persisted log entry: {:?} {} {:?} {:?}:{:?} {}",
                    now,
                    record.level(),
                    record.module_path_static(),
                    record.file_static(),
                    record.line(),
                    record.args(),
                );
            }
            return;
        }
        let entry = LogEntry {
            timestamp: now,
            hostname: self.hostname.clone(),
            level: record.level(),
            module: Some(record.module_path().unwrap_or("").to_owned()),
            filename: Some(record.file().unwrap_or("").to_owned()),
            line: record.line(),
            message: format!("{}", record.args()),
        };
        self.action_tx.send(Action::Record(entry)).unwrap();
    }

    fn flush(&self) {
        let done_rx = self.done_rx.lock().unwrap();
        self.action_tx.send(Action::Flush).unwrap();
        done_rx.recv().unwrap();
    }
}

/// Configures the global logger to use a new instance backed by the database connection `db`.
///
/// Logger configuration happens via environment variables and tries to respect the same
/// variables that `env_logger` recognizes.  Misconfigured variables result in a fatal error.
pub async fn init(db: Connection) -> Handle {
    let max_level = env_rust_log();

    let hostname =
        gethostname().into_string().unwrap_or_else(|_e| String::from("invalid-hostname"));

    let logger = DbLogger::new(hostname, db.clone(), Arc::from(SystemClock::default())).await;
    let handle =
        Handle { db, action_tx: logger.action_tx.clone(), done_rx: logger.done_rx.clone() };

    log::set_boxed_logger(Box::from(logger)).expect("Logger should not have been set up yet");
    log::set_max_level(max_level.to_level_filter());
    handle
}

#[cfg(test)]
#[cfg(feature = "sqlite")]
mod tests {
    //! Unit-tests for the database-backed logger.
    //!
    //! These tests do not verify the interaction with the `log` facade because it's impossible to
    //! do so in this context: the logger is a global resource and we cannot run independent tests
    //! for it.  See the separate `logger_test.rs` program for integration tests against the real
    //! test database.

    use super::*;
    use crate::clocks::MonotonicClock;
    use crate::sqlite;
    use log::RecordBuilder;

    /// Sets up the logger backing it with an in-memory database and a fake clock.
    async fn setup() -> (DbLogger, Connection) {
        let db = sqlite::connect(sqlite::ConnectionOptions { uri: ":memory:".to_owned() })
            .await
            .unwrap();
        db.create_schema().await.unwrap();
        let clock = Arc::from(MonotonicClock::new(1000));
        (DbLogger::new("fake-hostname".to_owned(), db.clone(), clock).await, db)
    }

    /// Emits one single log entry at every possible level.
    fn emit_all_log_levels(logger: &dyn Log) {
        for (level, message) in &[
            (Level::Error, "An error message"),
            (Level::Warn, "A warning message"),
            (Level::Info, "An info message"),
            (Level::Debug, "A debug message"),
            (Level::Trace, "A trace message"),
        ] {
            logger.log(
                &RecordBuilder::new()
                    .level(*level)
                    .module_path_static(Some("the-module"))
                    .file_static(Some("the-file"))
                    .line(Some(123))
                    .args(format_args!("{}", message))
                    .build(),
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_all_log_levels() {
        let (logger, db) = setup().await;

        emit_all_log_levels(&logger);

        logger.flush();
        let entries = db.0.get_log_entries().await.unwrap();
        assert_eq!(
            vec![
                "1000.0 fake-hostname 1 the-module the-file:123 An error message".to_owned(),
                "1001.0 fake-hostname 2 the-module the-file:123 A warning message".to_owned(),
                "1002.0 fake-hostname 3 the-module the-file:123 An info message".to_owned(),
                "1003.0 fake-hostname 4 the-module the-file:123 A debug message".to_owned(),
                "1004.0 fake-hostname 5 the-module the-file:123 A trace message".to_owned(),
            ],
            entries
        );
    }
}
