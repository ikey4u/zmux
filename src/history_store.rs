use std::{
    collections::HashSet,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, ErrorCode, OpenFlags, Params};

use crate::types::{CellStyle, SnapshotLine};

pub const DEFAULT_MAX_HISTORY_LINES: u64 = 1_000_000;

const SCHEMA_VERSION: i64 = 2;
const STYLE_BLOB_VERSION: u8 = 1;
const HISTORY_WRITE_QUEUE_BATCHES: usize = 8;
const HISTORY_WRITE_BATCH_LINES: usize = 1_000;
const SQLITE_BUSY_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(25),
    Duration::from_millis(100),
    Duration::from_millis(400),
];

static NEXT_HISTORY_PANE_KEY: AtomicU64 = AtomicU64::new(1);
static HISTORY_RUN_ID: OnceLock<String> = OnceLock::new();
static CLEANED_STATE_DIRECTORIES: OnceLock<Mutex<HashSet<PathBuf>>> =
    OnceLock::new();
// Pane writers remain independently bounded, but transactions in this zmux
// process are serialized before they reach the shared SQLite file. SQLite WAL
// then coordinates the rare case of two independent zmux servers using the
// same user-level database.
static GLOBAL_HISTORY_WRITE_LOCK: Mutex<()> = Mutex::new(());

pub type HistoryStoreResult<T> = Result<T, HistoryStoreError>;

#[derive(Debug)]
pub enum HistoryStoreError {
    Io(io::Error),
    Sql(rusqlite::Error),
    CorruptData(String),
    IdExhausted,
    LimitTooLarge,
}

impl fmt::Display for HistoryStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "history storage I/O error: {error}"),
            Self::Sql(error) => {
                write!(f, "history storage SQLite error: {error}")
            }
            Self::CorruptData(message) => {
                write!(f, "history storage contains corrupt data: {message}")
            }
            Self::IdExhausted => {
                write!(f, "history line id space is exhausted")
            }
            Self::LimitTooLarge => write!(f, "history page limit is too large"),
        }
    }
}

impl Error for HistoryStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sql(error) => Some(error),
            Self::CorruptData(_) | Self::IdExhausted | Self::LimitTooLarge => {
                None
            }
        }
    }
}

impl From<io::Error> for HistoryStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for HistoryStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

fn retry_transient_sqlite_busy<T>(
    mut operation: impl FnMut() -> HistoryStoreResult<T>,
) -> HistoryStoreResult<T> {
    for delay in SQLITE_BUSY_RETRY_DELAYS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_transient_sqlite_busy(&error) => {
                thread::sleep(delay);
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

fn is_transient_sqlite_busy(error: &HistoryStoreError) -> bool {
    matches!(
        error,
        HistoryStoreError::Sql(rusqlite::Error::SqliteFailure(
            sqlite_error,
            _
        )) if matches!(sqlite_error.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHistoryLine {
    pub id: u64,
    pub text: String,
    pub terminated: bool,
    pub styles: Vec<CellStyle>,
}

impl StoredHistoryLine {
    pub fn into_snapshot_line(self) -> SnapshotLine {
        SnapshotLine {
            text: self.text,
            terminated: self.terminated,
            styles: self.styles,
        }
    }
}

/// Disk-backed scrollback facade for one pane.
///
/// All panes use one private `zmux.sqlite3` file per user state directory.
/// `run_id` and `pane_key` make each pane's rows private even when multiple
/// sessions or independent zmux server processes happen to reuse their numeric
/// pane identifiers. The database is created lazily on the first non-empty
/// append; closing a pane deletes only that pane's rows, not the shared file.
pub struct PaneHistory {
    state_dir: PathBuf,
    run_id: String,
    pane_key: u64,
    max_lines: u64,
    next_id: u64,
    stored_lines: u64,
    database_path: Option<PathBuf>,
    connection: Option<Connection>,
}

enum HistoryWriterCommand {
    Append(Vec<SnapshotLine>),
    Barrier(mpsc::Sender<()>),
    Reset,
    Discard(mpsc::Sender<()>),
}

/// Bounded write-behind queue for a pane's cold history.
///
/// PTY readers only enqueue parsed logical lines; SQLite transactions run on
/// this worker. A barrier lets copy mode observe an atomic hot/cold boundary.
/// The bounded queue caps per-pane memory without applying disk backpressure
/// to the PTY reader. If the queue fills or disk storage fails, the cold tier
/// is discarded and disabled rather than delaying interactive terminal I/O or
/// retrying the same growing buffer forever.
#[derive(Clone)]
pub struct PaneHistoryWriter {
    sender: SyncSender<HistoryWriterCommand>,
    failed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl PaneHistoryWriter {
    pub fn start(history: Arc<Mutex<PaneHistory>>) -> Self {
        let (sender, receiver) =
            mpsc::sync_channel(HISTORY_WRITE_QUEUE_BATCHES);
        let failed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        let worker_stopped = Arc::clone(&stopped);

        thread::spawn(move || {
            let mut discarded = false;
            let mut pending_command = None;
            loop {
                let command = match pending_command.take() {
                    Some(command) => command,
                    None => match receiver.recv() {
                        Ok(command) => command,
                        Err(_) => break,
                    },
                };
                match command {
                    HistoryWriterCommand::Append(mut lines) => {
                        // A busy terminal usually produces many small parser
                        // captures. Combine adjacent captures into one SQLite
                        // transaction, but never move a barrier or reset across
                        // an append boundary.
                        while lines.len() < HISTORY_WRITE_BATCH_LINES {
                            match receiver.try_recv() {
                                Ok(HistoryWriterCommand::Append(mut next)) => {
                                    lines.append(&mut next);
                                }
                                Ok(command) => {
                                    pending_command = Some(command);
                                    break;
                                }
                                Err(mpsc::TryRecvError::Empty)
                                | Err(mpsc::TryRecvError::Disconnected) => {
                                    break;
                                }
                            }
                        }
                        if worker_failed.load(Ordering::Relaxed)
                            || worker_stopped.load(Ordering::Relaxed)
                        {
                            discard_failed_history(&history, &mut discarded);
                            continue;
                        }
                        let append_ok = history
                            .lock()
                            .map(|mut history| {
                                history.append_batch(&lines).is_ok()
                            })
                            .unwrap_or(false);
                        if !append_ok {
                            if !worker_failed.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "zmux: disabling disk scrollback after a history write failure"
                                );
                            }
                            discard_failed_history(&history, &mut discarded);
                        }
                    }
                    HistoryWriterCommand::Barrier(ack) => {
                        if worker_failed.load(Ordering::Relaxed) {
                            discard_failed_history(&history, &mut discarded);
                        }
                        let _ = ack.send(());
                    }
                    HistoryWriterCommand::Reset => {
                        if worker_failed.load(Ordering::Relaxed) {
                            discard_failed_history(&history, &mut discarded);
                            continue;
                        }
                        let clear_ok = history
                            .lock()
                            .map(|mut history| history.clear().is_ok())
                            .unwrap_or(false);
                        if !clear_ok {
                            if !worker_failed.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "zmux: disabling disk scrollback after a history clear failure"
                                );
                            }
                            discard_failed_history(&history, &mut discarded);
                        }
                    }
                    HistoryWriterCommand::Discard(ack) => {
                        if let Ok(mut history) = history.lock() {
                            history.disable_and_discard();
                        }
                        let _ = ack.send(());
                        break;
                    }
                }
            }
        });

        Self {
            sender,
            failed,
            stopped,
        }
    }

    pub fn append(&self, lines: Vec<SnapshotLine>) {
        if lines.is_empty()
            || self.failed.load(Ordering::Relaxed)
            || self.stopped.load(Ordering::Relaxed)
        {
            return;
        }

        match self.sender.try_send(HistoryWriterCommand::Append(lines)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.disable_after_queue_failure("history write queue is full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.disable_after_queue_failure(
                    "history write worker disconnected",
                );
            }
        }
    }

    fn disable_after_queue_failure(&self, reason: &str) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            eprintln!("zmux: disabling disk scrollback because the {reason}");
        }
    }

    /// Wait until every append queued before this call has committed or the
    /// cold tier has been disabled. Callers use the pane history serial lock to
    /// prevent a newer PTY batch from crossing this barrier.
    pub fn flush(&self) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        let (ack_sender, ack_receiver) = mpsc::channel();
        if self
            .sender
            .send(HistoryWriterCommand::Barrier(ack_sender))
            .is_ok()
        {
            let _ = ack_receiver.recv();
        }
    }

    /// Queue a clear of committed and already queued cold history at one FIFO
    /// boundary. This never waits for SQLite; call `flush` afterward when the
    /// cleared state must be observed synchronously.
    pub fn clear(&self) {
        if self.failed.load(Ordering::Relaxed)
            || self.stopped.load(Ordering::Relaxed)
        {
            return;
        }
        match self.sender.try_send(HistoryWriterCommand::Reset) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.disable_after_queue_failure("history write queue is full");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.disable_after_queue_failure(
                    "history write worker disconnected",
                );
            }
        }
    }

    /// Stop the worker and synchronously remove this pane's cold-history rows
    /// from the shared database.
    pub fn shutdown_and_discard(&self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        let (ack_sender, ack_receiver) = mpsc::channel();
        if self
            .sender
            .send(HistoryWriterCommand::Discard(ack_sender))
            .is_ok()
        {
            let _ = ack_receiver.recv();
        }
    }
}

fn discard_failed_history(
    history: &Arc<Mutex<PaneHistory>>,
    discarded: &mut bool,
) {
    if *discarded {
        return;
    }
    if let Ok(mut history) = history.lock() {
        history.disable_and_discard();
    }
    *discarded = true;
}

impl PaneHistory {
    pub fn new() -> HistoryStoreResult<Self> {
        Self::with_max_lines(DEFAULT_MAX_HISTORY_LINES)
    }

    pub fn with_max_lines(max_lines: u64) -> HistoryStoreResult<Self> {
        Ok(Self::in_directory(history_state_dir()?, max_lines))
    }

    /// Create a no-op store for environments without a usable private state
    /// directory. Terminal panes must still be able to start in that case.
    pub fn disabled() -> Self {
        Self::in_directory(PathBuf::new(), 0)
    }

    fn in_directory(state_dir: PathBuf, max_lines: u64) -> Self {
        Self {
            state_dir,
            run_id: history_run_id().to_owned(),
            pane_key: NEXT_HISTORY_PANE_KEY.fetch_add(1, Ordering::Relaxed),
            max_lines,
            next_id: 1,
            stored_lines: 0,
            database_path: None,
            connection: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(state_dir: PathBuf, max_lines: u64) -> Self {
        Self::in_directory(state_dir, max_lines)
    }

    pub fn max_lines(&self) -> u64 {
        self.max_lines
    }

    pub fn len(&self) -> u64 {
        self.stored_lines
    }

    pub fn is_empty(&self) -> bool {
        self.stored_lines == 0
    }

    pub fn append(&mut self, line: &SnapshotLine) -> HistoryStoreResult<u64> {
        let mut ids = self.append_batch(std::slice::from_ref(line))?;
        Ok(ids.remove(0))
    }

    /// Append logical lines in one transaction and return their assigned ids.
    pub fn append_batch(
        &mut self,
        lines: &[SnapshotLine],
    ) -> HistoryStoreResult<Vec<u64>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let line_count = u64::try_from(lines.len())
            .map_err(|_| HistoryStoreError::IdExhausted)?;
        let final_id = self
            .next_id
            .checked_add(line_count - 1)
            .ok_or(HistoryStoreError::IdExhausted)?;
        if final_id > i64::MAX as u64 {
            return Err(HistoryStoreError::IdExhausted);
        }
        let following_id = final_id
            .checked_add(1)
            .ok_or(HistoryStoreError::IdExhausted)?;

        let encoded_styles = lines
            .iter()
            .map(encode_styles)
            .collect::<HistoryStoreResult<Vec<_>>>()?;
        let ids = (self.next_id..following_id).collect::<Vec<_>>();

        // A zero-sized store still hands out monotonic ids, but never creates a
        // database or retains data.
        if self.max_lines == 0 {
            self.next_id = following_id;
            return Ok(ids);
        }

        let _write_guard = GLOBAL_HISTORY_WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prospective_len = self
            .stored_lines
            .checked_add(line_count)
            .ok_or(HistoryStoreError::LimitTooLarge)?;
        let excess = prospective_len.saturating_sub(self.max_lines);
        let excess_sql = i64::try_from(excess)
            .map_err(|_| HistoryStoreError::LimitTooLarge)?;
        retry_transient_sqlite_busy(|| {
            self.ensure_open()?;
            self.append_open_batch(lines, &encoded_styles, &ids, excess_sql)
        })?;

        self.next_id = following_id;
        self.stored_lines = prospective_len - excess;
        Ok(ids)
    }

    fn append_open_batch(
        &mut self,
        lines: &[SnapshotLine],
        encoded_styles: &[Vec<u8>],
        ids: &[u64],
        excess_sql: i64,
    ) -> HistoryStoreResult<()> {
        let pane_key = i64::try_from(self.pane_key)
            .map_err(|_| HistoryStoreError::IdExhausted)?;
        let run_id = self.run_id.clone();
        let connection = self.connection.as_mut().expect("opened above");
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO history_lines \
                 (run_id, pane_key, id, text, terminated, styles) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for ((line, styles), id) in
                lines.iter().zip(encoded_styles).zip(ids)
            {
                statement.execute(params![
                    &run_id,
                    pane_key,
                    i64::try_from(*id)
                        .map_err(|_| HistoryStoreError::IdExhausted)?,
                    line.text,
                    i64::from(line.terminated),
                    styles,
                ])?;
            }
        }
        if excess_sql > 0 {
            transaction.execute(
                "DELETE FROM history_lines WHERE run_id = ?1 \
                     AND pane_key = ?2 AND id IN (\
                     SELECT id FROM history_lines \
                     WHERE run_id = ?1 AND pane_key = ?2 \
                     ORDER BY id ASC LIMIT ?3\
                 )",
                params![&run_id, pane_key, excess_sql],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Return the newest `limit` lines in chronological order.
    pub fn tail(
        &self,
        limit: usize,
    ) -> HistoryStoreResult<Vec<StoredHistoryLine>> {
        if limit == 0 || self.connection.is_none() {
            return Ok(Vec::new());
        }
        let limit = page_limit(limit)?;
        let mut lines = query_lines(
            self.connection.as_ref().expect("checked above"),
            "SELECT id, text, terminated, styles FROM history_lines \
             WHERE run_id = ?1 AND pane_key = ?2 \
             ORDER BY id DESC LIMIT ?3",
            params![
                &self.run_id,
                i64::try_from(self.pane_key)
                    .map_err(|_| HistoryStoreError::IdExhausted)?,
                limit,
            ],
        )?;
        lines.reverse();
        Ok(lines)
    }

    /// Return the nearest `limit` lines whose ids are strictly less than
    /// `before_id`, in chronological order.
    pub fn before(
        &self,
        before_id: u64,
        limit: usize,
    ) -> HistoryStoreResult<Vec<StoredHistoryLine>> {
        if limit == 0 || self.connection.is_none() || before_id <= 1 {
            return Ok(Vec::new());
        }
        if before_id > i64::MAX as u64 {
            return self.tail(limit);
        }
        let limit = page_limit(limit)?;
        let mut lines = query_lines(
            self.connection.as_ref().expect("checked above"),
            "SELECT id, text, terminated, styles FROM history_lines \
             WHERE run_id = ?1 AND pane_key = ?2 AND id < ?3 \
             ORDER BY id DESC LIMIT ?4",
            params![
                &self.run_id,
                i64::try_from(self.pane_key)
                    .map_err(|_| HistoryStoreError::IdExhausted)?,
                before_id as i64,
                limit,
            ],
        )?;
        lines.reverse();
        Ok(lines)
    }

    /// Return the first `limit` lines whose ids are strictly greater than
    /// `after_id`, in chronological order.
    pub fn after(
        &self,
        after_id: u64,
        limit: usize,
    ) -> HistoryStoreResult<Vec<StoredHistoryLine>> {
        if limit == 0
            || self.connection.is_none()
            || after_id >= i64::MAX as u64
        {
            return Ok(Vec::new());
        }
        let limit = page_limit(limit)?;
        let lines = query_lines(
            self.connection.as_ref().expect("checked above"),
            "SELECT id, text, terminated, styles FROM history_lines \
             WHERE run_id = ?1 AND pane_key = ?2 AND id > ?3 \
             ORDER BY id ASC LIMIT ?4",
            params![
                &self.run_id,
                i64::try_from(self.pane_key)
                    .map_err(|_| HistoryStoreError::IdExhausted)?,
                after_id as i64,
                limit,
            ],
        )?;
        Ok(lines)
    }

    /// Remove all retained lines. Ids are deliberately not reused so stale
    /// copy-mode anchors cannot resolve to unrelated new content.
    pub fn clear(&mut self) -> HistoryStoreResult<()> {
        if self.connection.is_none() {
            return Ok(());
        }
        let pane_key = i64::try_from(self.pane_key)
            .map_err(|_| HistoryStoreError::IdExhausted)?;
        let run_id = self.run_id.clone();
        let clear_result = {
            let _write_guard = GLOBAL_HISTORY_WRITE_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let connection = self.connection.as_mut().expect("checked above");
            retry_transient_sqlite_busy(|| {
                connection
                    .execute(
                        "DELETE FROM history_lines \
                         WHERE run_id = ?1 AND pane_key = ?2",
                        params![&run_id, pane_key],
                    )
                    .map_err(HistoryStoreError::from)
            })
        };
        if let Err(error) = clear_result {
            // A failed clear must never make old output reappear. Even if the
            // best-effort scoped delete inside `disable_and_discard` also
            // fails, this pane will never reopen the same scope.
            self.disable_and_discard();
            return Err(error);
        }
        self.stored_lines = 0;

        // The database uses incremental auto-vacuum. Reclaiming pages is best
        // effort because the logical clear has already committed at this point.
        if let Some(connection) = self.connection.as_mut() {
            let _ = connection.execute_batch("PRAGMA incremental_vacuum;");
        }
        Ok(())
    }

    fn reset_storage(&mut self) {
        let run_id = self.run_id.clone();
        let pane_key = i64::try_from(self.pane_key).unwrap_or(i64::MAX);
        if let Some(connection) = self.connection.as_mut() {
            let _write_guard = GLOBAL_HISTORY_WRITE_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = retry_transient_sqlite_busy(|| {
                connection
                    .execute(
                        "DELETE FROM history_lines \
                         WHERE run_id = ?1 AND pane_key = ?2",
                        params![&run_id, pane_key],
                    )
                    .map_err(HistoryStoreError::from)
            });
        }
        drop(self.connection.take());
        self.database_path.take();
        self.stored_lines = 0;
    }

    fn disable_and_discard(&mut self) {
        self.reset_storage();
        self.max_lines = 0;
        self.state_dir.clear();
    }

    fn ensure_open(&mut self) -> HistoryStoreResult<()> {
        if self.connection.is_some() {
            return Ok(());
        }

        prepare_state_directory(&self.state_dir)?;
        let path = reserve_database_file(&self.state_dir)?;

        let open_result = (|| -> HistoryStoreResult<Connection> {
            let connection = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.execute_batch(&format!(
                "PRAGMA journal_mode=WAL;\
                 PRAGMA synchronous=NORMAL;\
                 PRAGMA wal_autocheckpoint=1000;\
                 PRAGMA temp_store=MEMORY;\
                 PRAGMA auto_vacuum=INCREMENTAL;\
                 CREATE TABLE IF NOT EXISTS history_lines (\
                     run_id TEXT NOT NULL,\
                     pane_key INTEGER NOT NULL CHECK(pane_key > 0),\
                     id INTEGER NOT NULL CHECK(id > 0),\
                     text TEXT NOT NULL,\
                     terminated INTEGER NOT NULL \
                         CHECK(terminated IN (0, 1)),\
                     styles BLOB NOT NULL,\
                     PRIMARY KEY(run_id, pane_key, id)\
                 ) STRICT;\
                 PRAGMA user_version={SCHEMA_VERSION};"
            ))?;
            set_database_permissions(&path)?;
            cleanup_stale_history_once(
                &connection,
                &self.state_dir,
                &self.run_id,
            );
            Ok(connection)
        })();

        match open_result {
            Ok(connection) => {
                self.database_path = Some(path);
                self.connection = Some(connection);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for PaneHistory {
    fn drop(&mut self) {
        // SQLite must release Windows file handles after this pane's rows have
        // been removed. The shared database remains for other panes.
        self.reset_storage();
    }
}

fn query_lines<P: Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> HistoryStoreResult<Vec<StoredHistoryLine>> {
    let mut statement = connection.prepare_cached(sql)?;
    let rows = statement.query_map(params, |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;

    let mut lines = Vec::new();
    for row in rows {
        let (id, text, terminated, style_blob) = row?;
        let id = u64::try_from(id).map_err(|_| {
            HistoryStoreError::CorruptData(format!(
                "invalid non-positive line id {id}"
            ))
        })?;
        if id == 0 {
            return Err(HistoryStoreError::CorruptData(
                "line id must be positive".to_string(),
            ));
        }
        let terminated = match terminated {
            0 => false,
            1 => true,
            other => {
                return Err(HistoryStoreError::CorruptData(format!(
                    "invalid terminated value {other}"
                )))
            }
        };
        let styles = decode_styles(&style_blob, text.chars().count())?;
        lines.push(StoredHistoryLine {
            id,
            text,
            terminated,
            styles,
        });
    }
    Ok(lines)
}

fn encode_styles(line: &SnapshotLine) -> HistoryStoreResult<Vec<u8>> {
    let character_count = line.text.chars().count();
    if line.styles.len() > character_count {
        return Err(HistoryStoreError::CorruptData(format!(
            "{} styles for a {}-character line",
            line.styles.len(),
            character_count
        )));
    }
    let style_count = u32::try_from(line.styles.len()).map_err(|_| {
        HistoryStoreError::CorruptData(
            "too many styles in one line".to_string(),
        )
    })?;

    let mut blob = Vec::new();
    blob.push(STYLE_BLOB_VERSION);
    blob.extend_from_slice(&style_count.to_le_bytes());

    let mut start = 0;
    while start < line.styles.len() {
        let style = &line.styles[start];
        let mut end = start + 1;
        while end < line.styles.len() && line.styles[end] == *style {
            end += 1;
        }
        let run_length = u32::try_from(end - start).map_err(|_| {
            HistoryStoreError::CorruptData("style run is too long".to_string())
        })?;
        blob.extend_from_slice(&run_length.to_le_bytes());
        blob.push(style.flags);
        write_string(&mut blob, &style.fg)?;
        write_string(&mut blob, &style.bg)?;
        start = end;
    }
    Ok(blob)
}

fn decode_styles(
    blob: &[u8],
    character_count: usize,
) -> HistoryStoreResult<Vec<CellStyle>> {
    let mut reader = BlobReader::new(blob);
    let version = reader.read_u8()?;
    if version != STYLE_BLOB_VERSION {
        return Err(HistoryStoreError::CorruptData(format!(
            "unsupported style blob version {version}"
        )));
    }
    let style_count = usize::try_from(reader.read_u32()?).map_err(|_| {
        HistoryStoreError::CorruptData("style count is too large".to_string())
    })?;
    if style_count > character_count {
        return Err(HistoryStoreError::CorruptData(format!(
            "{style_count} styles for a {character_count}-character line"
        )));
    }

    let mut styles = Vec::with_capacity(style_count);
    while styles.len() < style_count {
        let run_length = usize::try_from(reader.read_u32()?).map_err(|_| {
            HistoryStoreError::CorruptData(
                "style run length is too large".to_string(),
            )
        })?;
        if run_length == 0 || run_length > style_count - styles.len() {
            return Err(HistoryStoreError::CorruptData(
                "invalid style run length".to_string(),
            ));
        }
        let flags = reader.read_u8()?;
        let fg = reader.read_string()?;
        let bg = reader.read_string()?;
        let style = CellStyle { fg, bg, flags };
        styles.resize(styles.len() + run_length, style);
    }
    if !reader.is_finished() {
        return Err(HistoryStoreError::CorruptData(
            "trailing bytes in style blob".to_string(),
        ));
    }
    Ok(styles)
}

fn write_string(blob: &mut Vec<u8>, value: &str) -> HistoryStoreResult<()> {
    let length = u32::try_from(value.len()).map_err(|_| {
        HistoryStoreError::CorruptData("style string is too long".to_string())
    })?;
    blob.extend_from_slice(&length.to_le_bytes());
    blob.extend_from_slice(value.as_bytes());
    Ok(())
}

struct BlobReader<'a> {
    blob: &'a [u8],
    offset: usize,
}

impl<'a> BlobReader<'a> {
    fn new(blob: &'a [u8]) -> Self {
        Self { blob, offset: 0 }
    }

    fn read_u8(&mut self) -> HistoryStoreResult<u8> {
        let byte = *self.blob.get(self.offset).ok_or_else(|| {
            HistoryStoreError::CorruptData("truncated style blob".to_string())
        })?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_u32(&mut self) -> HistoryStoreResult<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn read_string(&mut self) -> HistoryStoreResult<String> {
        let length = usize::try_from(self.read_u32()?).map_err(|_| {
            HistoryStoreError::CorruptData(
                "style string length is too large".to_string(),
            )
        })?;
        let bytes = self.read_bytes(length)?;
        let value = std::str::from_utf8(bytes).map_err(|_| {
            HistoryStoreError::CorruptData(
                "style string is not valid UTF-8".to_string(),
            )
        })?;
        Ok(value.to_string())
    }

    fn read_bytes(&mut self, length: usize) -> HistoryStoreResult<&'a [u8]> {
        let end = self.offset.checked_add(length).ok_or_else(|| {
            HistoryStoreError::CorruptData(
                "style blob offset overflow".to_string(),
            )
        })?;
        let bytes = self.blob.get(self.offset..end).ok_or_else(|| {
            HistoryStoreError::CorruptData("truncated style blob".to_string())
        })?;
        self.offset = end;
        Ok(bytes)
    }

    fn is_finished(&self) -> bool {
        self.offset == self.blob.len()
    }
}

fn page_limit(limit: usize) -> HistoryStoreResult<i64> {
    i64::try_from(limit).map_err(|_| HistoryStoreError::LimitTooLarge)
}

fn reserve_database_file(state_dir: &Path) -> io::Result<PathBuf> {
    let path = state_dir.join("zmux.sqlite3");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    configure_file_mode(&mut options);
    match options.open(&path) {
        Ok(file) => drop(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    set_database_permissions(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn configure_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn configure_file_mode(_options: &mut OpenOptions) {}

fn prepare_state_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn cleanup_stale_history_once(
    connection: &Connection,
    state_dir: &Path,
    current_run_id: &str,
) {
    let cleaned =
        CLEANED_STATE_DIRECTORIES.get_or_init(|| Mutex::new(HashSet::new()));
    let should_clean = cleaned
        .lock()
        .map(|mut directories| directories.insert(state_dir.to_path_buf()))
        .unwrap_or(false);
    if !should_clean {
        return;
    }

    cleanup_stale_history_rows(connection, current_run_id);
    cleanup_stale_legacy_history_files(state_dir);
}

fn cleanup_stale_history_rows(connection: &Connection, current_run_id: &str) {
    let mut statement = match connection
        .prepare("SELECT DISTINCT run_id FROM history_lines WHERE run_id <> ?1")
    {
        Ok(statement) => statement,
        Err(_) => return,
    };
    let run_ids = match statement
        .query_map(params![current_run_id], |row| row.get::<_, String>(0))
    {
        Ok(rows) => rows.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(_) => return,
    };
    drop(statement);

    for run_id in run_ids {
        let Some(process_id) = history_run_process_id(&run_id) else {
            continue;
        };
        if !history_owner_is_alive(process_id, Path::new(&run_id)) {
            let _ = connection.execute(
                "DELETE FROM history_lines WHERE run_id = ?1",
                params![run_id],
            );
        }
    }
}

fn cleanup_stale_legacy_history_files(state_dir: &Path) {
    let Ok(entries) = fs::read_dir(state_dir) else {
        return;
    };
    let mut database_paths = HashSet::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(sqlite_end) = file_name.find(".sqlite3") else {
            continue;
        };
        let base_name = &file_name[..sqlite_end + ".sqlite3".len()];
        if base_name.starts_with("pane-") {
            database_paths.insert(state_dir.join(base_name));
        }
    }

    for path in database_paths {
        let Some(process_id) = legacy_history_database_process_id(&path) else {
            continue;
        };
        if !history_owner_is_alive(process_id, &path) {
            cleanup_database_files(&path);
        }
    }
}

fn legacy_history_database_process_id(path: &Path) -> Option<u32> {
    let file_name = path.file_name()?.to_str()?;
    let mut parts = file_name.split('-');
    if parts.next()? != "pane" {
        return None;
    }
    parts.next()?.parse().ok()
}

fn history_run_id() -> &'static str {
    HISTORY_RUN_ID
        .get_or_init(|| {
            let process_id = std::process::id();
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("run-{process_id}-{timestamp:032x}")
        })
        .as_str()
}

fn history_run_process_id(run_id: &str) -> Option<u32> {
    let mut parts = run_id.split('-');
    if parts.next()? != "run" {
        return None;
    }
    parts.next()?.parse().ok()
}

#[cfg(unix)]
fn history_owner_is_alive(process_id: u32, _path: &Path) -> bool {
    if process_id == std::process::id() {
        return true;
    }
    let Ok(process_id) = i32::try_from(process_id) else {
        return false;
    };
    if process_id <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(process_id, 0) };
    result == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn history_owner_is_alive(process_id: u32, path: &Path) -> bool {
    if process_id == std::process::id() {
        return true;
    }
    // Without a portable process-liveness API, retain recent files in case a
    // second zmux server is active, and reclaim old crash leftovers.
    let Some(age) = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
    else {
        // Shared database rows have no corresponding file per run. Retaining
        // an uncertain stale scope is safer than deleting a live server on a
        // platform without a portable process-liveness check.
        return true;
    };
    age < Duration::from_secs(24 * 60 * 60)
}

#[cfg(unix)]
fn set_database_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_database_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn cleanup_database_files(path: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let candidate = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut name = OsString::from(path.as_os_str());
            name.push(suffix);
            PathBuf::from(name)
        };
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

#[cfg(target_os = "macos")]
fn history_state_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| missing_env("HOME"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("zmux")
        .join("history"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn history_state_dir() -> io::Result<PathBuf> {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state_home).join("zmux").join("history"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| missing_env("HOME"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("zmux")
        .join("history"))
}

#[cfg(windows)]
fn history_state_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .ok_or_else(|| missing_env("LOCALAPPDATA or APPDATA"))?;
    Ok(PathBuf::from(base).join("zmux").join("history"))
}

#[cfg(not(any(unix, windows)))]
fn history_state_dir() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no state directory is defined for this platform",
    ))
}

fn missing_env(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{name} is not set"))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn test_directory(name: &str) -> PathBuf {
        let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("zmux-history-{name}-{}-{id}", std::process::id()))
    }

    fn line(text: &str) -> SnapshotLine {
        SnapshotLine {
            text: text.to_string(),
            terminated: true,
            styles: Vec::new(),
        }
    }

    fn history(name: &str, max_lines: u64) -> (PathBuf, PaneHistory) {
        let directory = test_directory(name);
        let _ = fs::remove_dir_all(&directory);
        let history = PaneHistory::in_directory(directory.clone(), max_lines);
        (directory, history)
    }

    #[test]
    fn pages_before_after_and_tail_in_chronological_order() {
        let (directory, mut history) = history("pagination", 100);
        let lines = (1..=6)
            .map(|number| line(&format!("line {number}")))
            .collect::<Vec<_>>();
        assert_eq!(
            history.append_batch(&lines).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );

        let tail = history.tail(3).unwrap();
        assert_eq!(
            tail.iter().map(|line| line.id).collect::<Vec<_>>(),
            vec![4, 5, 6]
        );
        assert_eq!(
            history
                .before(4, 2)
                .unwrap()
                .iter()
                .map(|line| line.id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            history
                .after(3, 2)
                .unwrap()
                .iter()
                .map(|line| line.id)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );

        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn styles_round_trip_through_rle_blob() {
        let (directory, mut history) = history("styles", 100);
        let red = CellStyle {
            fg: "#ff0000".to_string(),
            bg: "default".to_string(),
            flags: 0b00010,
        };
        let blue = CellStyle {
            fg: "蓝".to_string(),
            bg: "#000000".to_string(),
            flags: 0b01100,
        };
        let original = SnapshotLine {
            text: "abcdef".to_string(),
            terminated: false,
            styles: vec![red.clone(), red.clone(), red, blue.clone(), blue],
        };

        let id = history.append(&original).unwrap();
        let stored = history.tail(1).unwrap().pop().unwrap();
        assert_eq!(stored.id, id);
        assert_eq!(stored.text, original.text);
        assert_eq!(stored.terminated, original.terminated);
        assert_eq!(stored.styles, original.styles);

        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn retention_keeps_newest_lines_and_ids_keep_increasing() {
        let (directory, mut history) = history("retention", 3);
        for number in 1..=5 {
            assert_eq!(
                history.append(&line(&number.to_string())).unwrap(),
                number
            );
        }

        assert_eq!(history.len(), 3);
        let retained = history.tail(10).unwrap();
        assert_eq!(
            retained.iter().map(|line| line.id).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(
            retained
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["3", "4", "5"]
        );

        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn clear_removes_lines_without_reusing_ids() {
        let (directory, mut history) = history("clear", 100);
        assert_eq!(history.append(&line("old one")).unwrap(), 1);
        assert_eq!(history.append(&line("old two")).unwrap(), 2);

        history.clear().unwrap();
        assert!(history.is_empty());
        assert!(history.tail(10).unwrap().is_empty());
        assert_eq!(history.append(&line("new")).unwrap(), 3);
        assert_eq!(history.tail(1).unwrap()[0].text, "new");

        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_clear_disables_the_scope_so_old_rows_cannot_reappear() {
        let (directory, mut history) = history("failed-clear", 100);
        history.append(&line("old")).unwrap();
        history
            .connection
            .as_mut()
            .unwrap()
            .execute_batch("DROP TABLE history_lines;")
            .unwrap();

        assert!(history.clear().is_err());
        assert_eq!(history.max_lines(), 0);
        assert!(history.database_path.is_none());
        assert_eq!(history.append(&line("new")).unwrap(), 2);
        assert!(history.tail(10).unwrap().is_empty());

        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn database_is_lazy_and_drop_cleans_only_its_pane_rows() {
        let (directory, mut history) = history("lifecycle", 100);
        assert!(!directory.exists());
        assert!(history.tail(10).unwrap().is_empty());
        assert!(!directory.exists());

        history.append(&line("created")).unwrap();
        let database_path = history.database_path.clone().unwrap();
        assert!(database_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&database_path).unwrap().permissions().mode()
                    & 0o777,
                0o600
            );
        }

        let mut sibling = PaneHistory::in_directory(directory.clone(), 100);
        sibling.append(&line("sibling")).unwrap();
        assert_eq!(sibling.database_path.as_ref(), Some(&database_path));
        drop(history);
        assert!(database_path.exists());
        assert_eq!(sibling.tail(10).unwrap()[0].text, "sibling");
        drop(sibling);
        assert!(database_path.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn shared_database_isolates_panes_and_retention() {
        let directory = test_directory("shared-isolation");
        let _ = fs::remove_dir_all(&directory);
        let mut first = PaneHistory::in_directory(directory.clone(), 2);
        let mut second = PaneHistory::in_directory(directory.clone(), 100);

        first.append(&line("first-1")).unwrap();
        second.append(&line("second-1")).unwrap();
        first.append(&line("first-2")).unwrap();
        second.append(&line("second-2")).unwrap();
        first.append(&line("first-3")).unwrap();

        assert_eq!(
            first
                .tail(10)
                .unwrap()
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>(),
            vec!["first-2", "first-3"]
        );
        assert_eq!(
            second
                .tail(10)
                .unwrap()
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>(),
            vec!["second-1", "second-2"]
        );

        first.clear().unwrap();
        assert!(first.tail(10).unwrap().is_empty());
        assert_eq!(second.tail(10).unwrap().len(), 2);
        let database_path = first.database_path.clone().unwrap();
        drop(first);
        assert!(database_path.exists());
        assert_eq!(second.tail(10).unwrap().len(), 2);
        drop(second);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn write_behind_worker_flushes_clears_and_discards_its_pane_scope() {
        let (directory, history) = history("writer", 10_000);
        let history = Arc::new(Mutex::new(history));
        let writer = PaneHistoryWriter::start(Arc::clone(&history));
        writer.append(
            (0..2_500)
                .map(|index| line(&format!("line {index}")))
                .collect(),
        );
        // Reset is queued after the append. The following barrier observes
        // both the queued data and the asynchronous clear.
        writer.clear();
        writer.flush();
        assert!(history.lock().unwrap().is_empty());
        let database_path = history
            .lock()
            .unwrap()
            .database_path
            .clone()
            .expect("writer should create a database");
        assert!(database_path.exists());

        writer.append(vec![line("after clear")]);
        writer.flush();
        let retained = history.lock().unwrap().tail(10).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].text, "after clear");

        writer.shutdown_and_discard();
        assert!(database_path.exists());
        assert!(history.lock().unwrap().tail(10).unwrap().is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn large_write_behind_append_returns_while_history_is_locked() {
        let (directory, history) = history("nonblocking-large-append", 20_000);
        let history = Arc::new(Mutex::new(history));
        let writer = PaneHistoryWriter::start(Arc::clone(&history));
        let history_guard = history.lock().unwrap();
        let line_count =
            (HISTORY_WRITE_QUEUE_BATCHES + 2) * HISTORY_WRITE_BATCH_LINES;
        let lines = (0..line_count)
            .map(|index| line(&format!("line {index}")))
            .collect();
        let producer_writer = writer.clone();
        let (returned_sender, returned_receiver) = mpsc::channel();
        let producer = thread::spawn(move || {
            producer_writer.append(lines);
            let _ = returned_sender.send(());
        });

        let returned_while_locked = returned_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        drop(history_guard);
        producer.join().unwrap();
        writer.flush();

        assert!(
            returned_while_locked,
            "a large append waited for the history worker's mutex"
        );
        assert!(!writer.failed.load(Ordering::Relaxed));
        assert_eq!(history.lock().unwrap().len(), line_count as u64);
        writer.shutdown_and_discard();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn write_behind_clear_returns_while_history_is_locked() {
        let (directory, history) = history("nonblocking-clear", 100);
        let history = Arc::new(Mutex::new(history));
        let writer = PaneHistoryWriter::start(Arc::clone(&history));
        let history_guard = history.lock().unwrap();
        // The append ahead of Reset guarantees the worker must acquire the
        // held history lock before it can reach the clear command.
        writer.append(vec![line("before clear")]);
        let clear_writer = writer.clone();
        let (returned_sender, returned_receiver) = mpsc::channel();
        let clearer = thread::spawn(move || {
            clear_writer.clear();
            let _ = returned_sender.send(());
        });

        let returned_while_locked = returned_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        drop(history_guard);
        clearer.join().unwrap();
        writer.flush();

        assert!(
            returned_while_locked,
            "clear waited for the history worker's mutex"
        );
        assert!(history.lock().unwrap().is_empty());
        writer.shutdown_and_discard();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn full_write_behind_queue_disables_and_discards_cold_history() {
        let (directory, history) = history("full-write-queue", 100);
        let history = Arc::new(Mutex::new(history));
        let writer = PaneHistoryWriter::start(Arc::clone(&history));
        let history_guard = history.lock().unwrap();
        let producer_writer = writer.clone();
        let (returned_sender, returned_receiver) = mpsc::channel();
        let producer = thread::spawn(move || {
            // The worker can coalesce at most one transaction's target size
            // before blocking on the held history lock, so this always sends
            // enough commands to exceed the remaining queue capacity.
            for index in
                0..(HISTORY_WRITE_BATCH_LINES + HISTORY_WRITE_QUEUE_BATCHES + 1)
            {
                producer_writer.append(vec![line(&format!("queued {index}"))]);
            }
            let _ = returned_sender.send(());
        });

        let returned_while_locked = returned_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        let queue_failed = writer.failed.load(Ordering::Relaxed);
        drop(history_guard);
        producer.join().unwrap();
        writer.flush();

        assert!(
            returned_while_locked,
            "a full history queue applied backpressure to the producer"
        );
        assert!(queue_failed, "queue overflow did not disable cold history");
        let history = history.lock().unwrap();
        assert_eq!(history.max_lines(), 0);
        assert!(history.is_empty());
        drop(history);
        writer.shutdown_and_discard();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn concurrent_pane_writers_share_one_database_without_leaking_rows() {
        let directory = test_directory("shared-writers");
        let _ = fs::remove_dir_all(&directory);
        let first = Arc::new(Mutex::new(PaneHistory::in_directory(
            directory.clone(),
            10_000,
        )));
        let second = Arc::new(Mutex::new(PaneHistory::in_directory(
            directory.clone(),
            10_000,
        )));
        let first_writer = PaneHistoryWriter::start(Arc::clone(&first));
        let second_writer = PaneHistoryWriter::start(Arc::clone(&second));

        first_writer.append(
            (0..2_500)
                .map(|index| line(&format!("first-{index}")))
                .collect(),
        );
        second_writer.append(
            (0..2_500)
                .map(|index| line(&format!("second-{index}")))
                .collect(),
        );
        first_writer.flush();
        second_writer.flush();

        let first_tail = first.lock().unwrap().tail(10).unwrap();
        let second_tail = second.lock().unwrap().tail(10).unwrap();
        assert!(first_tail
            .iter()
            .all(|line| line.text.starts_with("first-")));
        assert!(second_tail
            .iter()
            .all(|line| line.text.starts_with("second-")));
        let first_path = first.lock().unwrap().database_path.clone().unwrap();
        assert_eq!(
            second.lock().unwrap().database_path.as_ref(),
            Some(&first_path)
        );

        first_writer.shutdown_and_discard();
        assert!(first.lock().unwrap().tail(10).unwrap().is_empty());
        assert_eq!(second.lock().unwrap().tail(10).unwrap().len(), 10);
        second_writer.shutdown_and_discard();
        drop(first);
        drop(second);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn append_waits_for_an_independent_connection_holding_the_write_lock() {
        let (directory, history) = history("independent-writer", 100);
        let history = Arc::new(Mutex::new(history));
        history.lock().unwrap().append(&line("seed")).unwrap();
        let database_path =
            history.lock().unwrap().database_path.clone().unwrap();
        let lock_connection = Connection::open(&database_path).unwrap();
        lock_connection.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let writer_history = Arc::clone(&history);
        let writer = thread::spawn(move || {
            writer_history.lock().unwrap().append(&line("after lock"))
        });
        thread::sleep(Duration::from_millis(50));
        lock_connection.execute_batch("COMMIT;").unwrap();

        assert_eq!(writer.join().unwrap().unwrap(), 2);
        assert_eq!(
            history
                .lock()
                .unwrap()
                .tail(10)
                .unwrap()
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>(),
            vec!["seed", "after lock"]
        );
        drop(history);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn write_failure_disables_cold_history_without_retrying() {
        let directory = test_directory("writer-failure");
        if let Some(parent) = directory.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        File::create(&directory).unwrap();
        let history = Arc::new(Mutex::new(PaneHistory::in_directory(
            directory.clone(),
            100,
        )));
        let writer = PaneHistoryWriter::start(Arc::clone(&history));

        writer.append(vec![line("cannot persist")]);
        writer.flush();

        assert!(writer.failed.load(Ordering::Relaxed));
        assert_eq!(history.lock().unwrap().max_lines(), 0);
        writer.append(vec![line("must not queue forever")]);
        writer.flush();
        assert!(history.lock().unwrap().is_empty());

        writer.shutdown_and_discard();
        fs::remove_file(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_rows_and_legacy_database_sidecars_are_reclaimed() {
        let directory = test_directory("stale-cleanup");
        prepare_state_directory(&directory).unwrap();
        let mut history = PaneHistory::in_directory(directory.clone(), 100);
        history.append(&line("current")).unwrap();
        let stale_run_id = "run-4294967295-00000000000000000000000000000001";
        history
            .connection
            .as_mut()
            .unwrap()
            .execute(
                "INSERT INTO history_lines \
                 (run_id, pane_key, id, text, terminated, styles) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    stale_run_id,
                    1_i64,
                    1_i64,
                    "stale",
                    1_i64,
                    vec![1_u8, 0, 0, 0, 0]
                ],
            )
            .unwrap();
        cleanup_stale_history_rows(
            history.connection.as_ref().unwrap(),
            &history.run_id,
        );
        let stale_count: i64 = history
            .connection
            .as_ref()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM history_lines WHERE run_id = ?1",
                params![stale_run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_count, 0);

        let database = directory.join(
            "pane-4294967295-00000000000000000000000000000001-0000000000000001.sqlite3",
        );
        File::create(&database).unwrap();
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        File::create(&wal).unwrap();

        cleanup_stale_legacy_history_files(&directory);

        assert!(!database.exists());
        assert!(!wal.exists());
        drop(history);
        let _ = fs::remove_dir_all(directory);
    }
}
