//! Commands that outlive the turn that started them.
//!
//! A build, a test suite, a training job: work measured in hours rather than
//! in the two minutes a tool call is allowed. Until now such a command was
//! simply killed when it ran long, which is the worst of the options — the
//! work is lost *and* nothing says so.
//!
//! The alternative usually reached for is `nohup … &`, and it is worse than it
//! looks: the process has no name to refer to, no place its output collects,
//! and no state anyone can read, so the agent forgets it exists until it
//! happens to knock. The three things a long process needs are exactly those,
//! and the two shipping implementations of this both provide them — one as a
//! manager of interactive terminals, the other as background tasks with an id
//! and a log file.
//!
//! Here they cost almost nothing, because each is something the project
//! already has. The name is a file under the project's state directory; the
//! output is an ordinary file, so `read` is all that is needed to look at it;
//! and the state is *derived* — a run is finished exactly when its exit file
//! exists — which is the same trick presence and the board already use.
//!
//! Every command starts this way, whether or not it is expected to be slow.
//! That is what makes running out of time a demotion rather than a death: the
//! log is already collecting and the run is already registered, so waiting no
//! longer is all "moving it to the background" has to mean.

use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::{ChildStdin, Command},
    sync::{mpsc, oneshot},
};

use crate::{Error, Result};

/// Two commands started in the same second from the same process would
/// otherwise share an id, and the second would overwrite the first's log.
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The runs this process can still speak to. Everything else about a run is
/// derived from files, so a restart loses nothing but this: a pipe cannot be
/// re-opened, which is why typing to a recovered run is refused rather than
/// silently dropped.
static LIVE: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<ChildStdin>>>>> =
    LazyLock::new(Mutex::default);

/// A run may live for days. Its log is useful, but not entitled to the disk.
const MAX_RUN_LOG: u64 = 2 * 1024 * 1024;
/// Below the bound on any tool result, with room for the markers, so the cut
/// a long output gets is this one — the one that names the log.
pub const MAX_RUN_OUTPUT: u64 = 48 * 1024;
/// Logs kept after their run because what was handed over had to be cut.
/// Enough to go back to the last few; beyond that the disk is not theirs.
const KEPT_LOGS: usize = 8;
const MAX_ACTIVE_RUNS: usize = 8;
const LOG_TRUNCATED: &[u8] = b"[older run output truncated]\n";
/// How often a wait looks again. The answer is on disk, so following a run is
/// reading a file that grows; nothing is gained by being cleverer than that.
const POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub command: String,
    pub started: u64,
    #[serde(default)]
    pub pid: Option<u32>,
    /// How far into the log the agent has already been shown. Kept with the
    /// run rather than in memory, so a second wait continues where the first
    /// stopped even across the restart the rest of this design survives.
    #[serde(default)]
    pub read: u64,
}

/// What one look at a run saw: whatever it wrote since the last look, and how
/// it stands now.
pub struct Progress {
    pub output: String,
    pub exit: Option<i32>,
}

pub struct StartedRun {
    pub run: Run,
    finished: oneshot::Receiver<std::result::Result<i32, String>>,
}

impl StartedRun {
    pub async fn wait(&mut self) -> Result<i32> {
        (&mut self.finished)
            .await
            .map_err(|_| Error::Tool("run supervisor stopped before the command finished".into()))?
            .map_err(Error::Tool)
    }
}

pub fn directory(cwd: &Path) -> PathBuf {
    elmee_plugin_paths::project_state_directory(cwd).join("runs")
}

fn log_path_in(base: &Path, id: &str) -> PathBuf {
    base.join(format!("{id}.log"))
}

fn exit_path_in(base: &Path, id: &str) -> PathBuf {
    base.join(format!("{id}.exit"))
}

fn record_path_in(base: &Path, id: &str) -> PathBuf {
    base.join(format!("{id}.json"))
}

/// The exit status, once there is one. `None` means still running.
pub fn outcome(cwd: &Path, id: &str) -> Option<i32> {
    outcome_in(&directory(cwd), id)
}

fn outcome_in(base: &Path, id: &str) -> Option<i32> {
    fs::read_to_string(exit_path_in(base, id)).ok().and_then(|code| code.trim().parse().ok())
}

pub fn list(cwd: &Path) -> Vec<Run> {
    let abandoned = abandoned();
    list_in(&directory(cwd)).into_iter().filter(|run| !abandoned.contains(&run.id)).collect()
}

/// Runs ended because whoever was waiting on them stopped waiting. They are
/// nobody's any more: not listed, and not reported when they exit.
static ABANDONED: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(Default::default);

fn abandoned() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    ABANDONED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// End a run that a stopped turn was waiting on, and say nothing about it.
///
/// Stopping an agent has to stop what it was doing. A command left running
/// would finish on its own later, be reported like any other, and wake the
/// agent that a person had just told to stop.
pub fn abandon(cwd: &Path, id: &str) {
    let _ = stop(cwd, id, false);
    abandoned().insert(id.to_owned());
    let (cwd, id) = (cwd.to_owned(), id.to_owned());
    tokio::spawn(async move {
        for _ in 0..40 {
            if outcome(&cwd, &id).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Asked first, and told if it would not listen.
        if outcome(&cwd, &id).is_none() {
            abandoned().remove(&id);
            let _ = stop(&cwd, &id, true);
            abandoned().insert(id.clone());
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        forget(&cwd, &id);
        abandoned().remove(&id);
    });
}

/// End every one of `ids` still running: asked now, made to after a grace
/// period. It runs from destructors, so the second step is off the runtime.
pub fn end(cwd: &Path, ids: &[String]) {
    let live: Vec<String> = ids.iter().filter(|id| outcome(cwd, id).is_none()).cloned().collect();
    for id in &live {
        let _ = stop(cwd, id, false);
    }
    let cwd = cwd.to_owned();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        for id in live.iter().filter(|id| outcome(&cwd, id).is_none()) {
            let _ = stop(&cwd, id, true);
        }
    });
}

fn list_in(base: &Path) -> Vec<Run> {
    let Ok(entries) = fs::read_dir(base) else { return Vec::new() };
    let mut runs: Vec<Run> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "json"))
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|data| serde_json::from_slice(&data).ok())
        .collect();
    runs.sort_by_key(|run| run.started);
    runs
}

/// Start `command` detached, collecting its output into the run's log.
///
/// The child gets its own process group. Without that it would sit in the
/// terminal's foreground group and Ctrl+C — which a person presses to stop the
/// *agent* — would also kill a training job started hours earlier.
pub async fn start(cwd: &Path, command: &str) -> Result<StartedRun> {
    start_in(cwd, &directory(cwd), command, true).await
}

async fn start_in(cwd: &Path, base: &Path, command: &str, grouped: bool) -> Result<StartedRun> {
    let slot = claim_slot_in(base)?;
    let started =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    fs::create_dir_all(base)?;
    let id = format!(
        "{started}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let log = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(log_path_in(base, &id))
        .await?;
    let mut process = Command::new(elmee_plugin_process::shell());
    elmee_plugin_process::env::apply(process.as_std_mut());
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        // A pipe rather than nothing, so a command that stops to ask something
        // can be answered. The cost is that a command reading input nobody
        // sends waits instead of seeing end-of-input — which is the demotion
        // this whole module is built around, not a hang.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if grouped {
        process.process_group(0);
    }
    let mut child = process.spawn().map_err(|error| Error::Tool(format!("cannot start command: {error}")))?;
    let stdout = child.stdout.take().ok_or_else(|| Error::Tool("run has no stdout".into()))?;
    let stderr = child.stderr.take().ok_or_else(|| Error::Tool("run has no stderr".into()))?;
    if let Some(stdin) = child.stdin.take() {
        live().insert(id.clone(), Arc::new(tokio::sync::Mutex::new(stdin)));
    }
    let run = Run { id: id.clone(), command: command.to_owned(), started, pid: child.id(), read: 0 };
    let record = record_path_in(base, &id);
    fs::write(&record, serde_json::to_vec(&run).unwrap_or_default())?;
    let exit = exit_path_in(base, &id);
    let (finished_tx, finished) = oneshot::channel();
    tokio::spawn(async move {
        let _slot = slot;
        let _stdin = Closing(id);
        let (tx, rx) = mpsc::channel(16);
        let stdout = drain(stdout, tx.clone());
        let stderr = drain(stderr, tx);
        let spool = spool(log, rx);
        let (status, stdout, stderr, spool) = tokio::join!(child.wait(), stdout, stderr, spool);
        let result = status.map_err(|error| format!("cannot wait for command: {error}")).and_then(|status| {
            let code = status.code().unwrap_or(128);
            // Completion is durable even if collecting its diagnostics failed;
            // otherwise one bad log write leaves a finished run "running" forever.
            let recorded = record_exit(&exit, code);
            (|| {
                stdout.map_err(|error| format!("cannot read command stdout: {error}"))?;
                stderr.map_err(|error| format!("cannot read command stderr: {error}"))?;
                spool.map_err(|error| format!("cannot write command log: {error}"))?;
                recorded.map_err(|error| format!("cannot record command exit: {error}"))?;
                Ok(code)
            })()
        });
        let _ = finished_tx.send(result);
    });
    Ok(StartedRun { run, finished })
}

fn live() -> std::sync::MutexGuard<'static, HashMap<String, Arc<tokio::sync::Mutex<ChildStdin>>>> {
    LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Closes the run's end of the pipe when its supervisor stops, whatever
/// stopped it. A writer left in the map would be a promise the process cannot
/// keep.
struct Closing(String);

impl Drop for Closing {
    fn drop(&mut self) {
        live().remove(&self.0);
    }
}

/// Send `input` to a live run. Only this process holds the pipe, so a run
/// recovered from disk is refused in as many words rather than swallowing it.
pub async fn feed(cwd: &Path, id: &str, input: &str, newline: bool) -> Result<()> {
    let stdin = live().get(id).cloned().ok_or_else(|| {
        Error::Tool(if list(cwd).iter().any(|run| run.id == id) {
            format!(
                "run {id} was started before this session, so its input can no longer be reached; \
                 stop it and start it again if you need to type to it"
            )
        } else {
            format!("no live run {id}")
        })
    })?;
    let mut stdin = stdin.lock().await;
    let mut bytes = input.as_bytes().to_vec();
    if newline {
        bytes.push(b'\n');
    }
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| Error::Tool(format!("cannot write to run {id}: {error}")))?;
    stdin.flush().await.map_err(|error| Error::Tool(format!("cannot write to run {id}: {error}")))?;
    Ok(())
}

/// Watch a run until it writes something new, ends, or `wait` runs out.
///
/// Polling, because the state is already two files: a log that grows and an
/// exit that appears. That makes following a run this process did not start
/// exactly as reliable as following one it did.
pub async fn follow(cwd: &Path, id: &str, wait: Duration, limit: u64) -> Result<Progress> {
    let base = directory(cwd);
    let record = record_path_in(&base, id);
    let mut run: Run =
        fs::read(&record).ok().and_then(|data| serde_json::from_slice(&data).ok()).ok_or_else(|| {
            Error::Tool(format!(
                "no run {id} is waiting to be read; either the id is \
                                            wrong or the run ended and its output was already handed over"
            ))
        })?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // The exit file is written after the log is complete, so reading the
        // log second can never miss a finished run's last words.
        let exit = outcome_in(&base, id);
        let (output, read) = read_since(&log_path_in(&base, id), run.read, limit);
        if !output.is_empty() || exit.is_some() || tokio::time::Instant::now() >= deadline {
            match exit {
                // Handed over once: the inbox must not report an ending the
                // agent has already been told about to its face.
                Some(_) => forget_in(&base, id),
                None => {
                    run.read = read;
                    let _ = fs::write(&record, serde_json::to_vec(&run).unwrap_or_default());
                }
            }
            return Ok(Progress { output, exit });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The log past `from`, and where the next look should start. A rotated log is
/// shorter than the offset; re-reading it is the honest answer, because what
/// stood between the two is gone.
fn read_since(path: &Path, from: u64, limit: u64) -> (String, u64) {
    read_span(path, from, limit).map_or((String::new(), from), |(text, length, _)| (text, length))
}

/// The log from `from` to its end in about `limit` bytes, and whether it was
/// cut. Over the limit it keeps both ends: a command says what it is doing
/// first and how it went last — a page its address, a build its failure — so
/// neither end alone is the useful one. The log is named last, where a later
/// cut that keeps the ends still keeps it, so the rest can be searched there.
fn read_span(path: &Path, from: u64, limit: u64) -> std::io::Result<(String, u64, bool)> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let from = if length < from { 0 } else { from };
    let mut read = |start: u64, bytes: u64| -> std::io::Result<String> {
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = Vec::new();
        (&mut file).take(bytes).read_to_end(&mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    };
    if length - from <= limit {
        return Ok((read(from, limit)?, length, false));
    }
    let half = limit / 2;
    let text = format!(
        "{}\n[… {} bytes left out here …]\n{}\n[the run's log, to search or read for the rest: {}]",
        read(from, half)?,
        length - from - 2 * half,
        read(length - half, half)?,
        path.display()
    );
    Ok((text, length, true))
}

fn claim_slot_in(base: &Path) -> Result<fs::File> {
    let slots = base.join("slots");
    fs::create_dir_all(&slots)?;
    for number in 0..MAX_ACTIVE_RUNS {
        let slot = fs::OpenOptions::new().create(true).append(true).open(slots.join(number.to_string()))?;
        match slot.try_lock() {
            Ok(()) => return Ok(slot),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Err(Error::Tool(format!(
        "this project already has {MAX_ACTIVE_RUNS} active runs; stop one before starting another"
    )))
}

/// Stop the whole process group recorded for a live run.
pub fn stop(cwd: &Path, id: &str, force: bool) -> Result<()> {
    let run = list(cwd)
        .into_iter()
        .find(|run| run.id == id)
        .ok_or_else(|| Error::Tool(format!("unknown run {id}")))?;
    if outcome(cwd, id).is_some() {
        return Err(Error::Tool(format!("run {id} has already finished")));
    }
    let raw =
        run.pid.ok_or_else(|| Error::Tool(format!("run {id} predates process control and has no pid")))?;
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_raw(raw as i32)
            .ok_or_else(|| Error::Tool(format!("run {id} has an invalid pid")))?;
        let signal = if force { rustix::process::Signal::KILL } else { rustix::process::Signal::TERM };
        rustix::process::kill_process_group(pid, signal)
            .map_err(|error| Error::Tool(format!("cannot stop run {id}: {error}")))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (raw, force);
        Err(Error::Tool("stopping a run is supported on macOS and Linux".into()))
    }
}

/// Atomic, so a reader never sees half a status, and not synced: a sync is
/// eight milliseconds on every command, for a file that is handed over and
/// removed a moment later. Power lost in between leaves a run that reads as
/// still going, which is what it was when the machine stopped.
fn record_exit(path: &Path, code: i32) -> std::io::Result<()> {
    let options = elmee_plugin_atomic::Options { durable: false, ..Default::default() };
    elmee_plugin_atomic::write(path, code.to_string().as_bytes(), options)
}

async fn drain(mut stream: impl AsyncRead + Unpin, sender: mpsc::Sender<Vec<u8>>) -> std::io::Result<()> {
    let mut buffer = vec![0_u8; 8192];
    loop {
        let bytes = stream.read(&mut buffer).await?;
        if bytes == 0 {
            return Ok(());
        }
        if sender.send(buffer[..bytes].to_vec()).await.is_err() {
            return Ok(());
        }
    }
}

/// A rotating tail on disk: output remains visible while the process runs,
/// but repeated output can never grow the state directory without limit.
async fn spool(mut log: tokio::fs::File, mut receiver: mpsc::Receiver<Vec<u8>>) -> std::io::Result<()> {
    let mut length = 0_u64;
    while let Some(chunk) = receiver.recv().await {
        if length + chunk.len() as u64 > MAX_RUN_LOG {
            let keep = MAX_RUN_LOG / 2;
            let start = length.saturating_sub(keep);
            log.seek(SeekFrom::Start(start)).await?;
            let mut tail = Vec::with_capacity(keep as usize);
            (&mut log).take(keep).read_to_end(&mut tail).await?;
            log.seek(SeekFrom::Start(0)).await?;
            log.set_len(0).await?;
            log.write_all(LOG_TRUNCATED).await?;
            log.write_all(&tail).await?;
            length = LOG_TRUNCATED.len() as u64 + tail.len() as u64;
        }
        log.write_all(&chunk).await?;
        length += chunk.len() as u64;
        log.flush().await?;
    }
    Ok(())
}

/// Everything a finished run left behind — its output, and how it ended —
/// handed over once, after which the run is forgotten. A log whose output had
/// to be cut is kept, since the output names it as the place for the rest.
pub fn take_output(cwd: &Path, id: &str, limit: u64) -> String {
    take_output_in(&directory(cwd), id, limit)
}

fn take_output_in(base: &Path, id: &str, limit: u64) -> String {
    let (log, cut) = read_span(&log_path_in(base, id), 0, limit)
        .map_or((String::new(), false), |(log, _, cut)| (log, cut));
    let output = match outcome_in(base, id) {
        Some(0) | None => log,
        Some(code) => format!("{log}\n[exit status: {code}]"),
    };
    let log = (!cut).then(|| log_path_in(base, id));
    for path in [exit_path_in(base, id), record_path_in(base, id)].into_iter().chain(log) {
        let _ = fs::remove_file(path);
    }
    if cut {
        prune_kept_logs(base);
    }
    output
}

/// Drop kept logs beyond the newest few. A kept log is one with no record
/// beside it; ids start with the second they were made, so name order is age.
fn prune_kept_logs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else { return };
    let mut kept: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .filter(|path| !path.with_extension("json").exists())
        .collect();
    kept.sort();
    for path in kept.iter().rev().skip(KEPT_LOGS) {
        let _ = fs::remove_file(path);
    }
}

fn forget(cwd: &Path, id: &str) {
    forget_in(&directory(cwd), id);
}

fn forget_in(base: &Path, id: &str) {
    for path in [log_path_in(base, id), exit_path_in(base, id), record_path_in(base, id)] {
        let _ = fs::remove_file(path);
    }
}

/// One line per run, for a person watching.
pub fn summary(cwd: &Path) -> String {
    summary_in(&directory(cwd))
}

fn summary_in(base: &Path) -> String {
    let runs = list_in(base);
    if runs.is_empty() {
        return "nothing running".into();
    }
    runs.iter()
        .map(|run| {
            let state = match outcome_in(base, &run.id) {
                None => "running".to_owned(),
                Some(0) => "done".to_owned(),
                Some(code) => format!("failed {code}"),
            };
            format!("{} [{state}] {}", run.id, run.command.lines().next().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_run_records_how_it_ended_even_when_the_command_exits_itself() {
        let cwd = std::env::temp_dir().join(format!("elmee-runs-{}", std::process::id()));
        let base = cwd.join("runs");
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();
        // The supervisor is what this test owns; process-group behaviour has its
        // own tests and must not make this one red.
        let mut first = start_in(&cwd, &base, "true", false).await.unwrap();
        let mut started = start_in(&cwd, &base, "printf hello; exit 3", false).await.unwrap();
        let run = started.run.clone();
        assert_ne!(first.run.id, run.id, "two runs in the same second are still two runs");
        assert_eq!(first.wait().await.unwrap(), 0);
        assert_eq!(started.wait().await.unwrap(), 3);
        assert_eq!(outcome_in(&base, &run.id), Some(3));
        assert!(summary_in(&base).contains("failed 3"));
        let output = take_output_in(&base, &run.id, MAX_RUN_OUTPUT);
        assert!(output.contains("hello"), "{output}");
        assert!(output.contains("exit status: 3"), "{output}");
        take_output_in(&base, &first.run.id, MAX_RUN_OUTPUT);
        assert_eq!(summary_in(&base), "nothing running");
        assert!(!log_path_in(&base, &run.id).exists(), "output handed over whole leaves nothing behind");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn output_over_the_limit_keeps_both_ends_and_names_the_log_it_keeps() {
        let cwd = std::env::temp_dir().join(elmee_core::unique("run-ends"));
        let base = cwd.join("runs");
        fs::create_dir_all(&base).unwrap();
        let mut started =
            start_in(&cwd, &base, "echo https://shop.example/item; seq 1 5000; echo FAILED", false)
                .await
                .unwrap();
        started.wait().await.unwrap();
        let output = take_output_in(&base, &started.run.id, 1_000);
        let log = log_path_in(&base, &started.run.id);
        assert!(output.starts_with("https://shop.example/item\n1\n2\n"), "{output}");
        assert!(output.contains("bytes left out here"), "{output}");
        assert!(output.contains("5000\nFAILED\n"), "{output}");
        assert!(output.ends_with(&format!("{}]", log.display())), "{output}");
        assert!(output.len() < 1_000 + 300, "{}", output.len());
        assert!(log.exists() && list_in(&base).is_empty(), "the log stays, the run does not");

        for second in 0..KEPT_LOGS + 2 {
            fs::write(base.join(format!("{second:010}-1-1.log")), "old").unwrap();
        }
        prune_kept_logs(&base);
        let logs = fs::read_dir(&base)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "log"))
            .count();
        assert_eq!(logs, KEPT_LOGS);
        assert!(log.exists(), "the newest kept log is the last to go");
        fs::remove_dir_all(cwd).unwrap();
    }

    #[tokio::test]
    async fn a_live_run_log_rotates_before_it_can_grow_without_limit() {
        let path = std::env::temp_dir().join(format!("elmee-run-spool-{}", std::process::id()));
        let file =
            fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        let (sender, receiver) = mpsc::channel(16);
        let writer = tokio::spawn(spool(tokio::fs::File::from_std(file), receiver));
        for index in 0..300 {
            sender.send(vec![b'a' + (index % 26) as u8; 8192]).await.unwrap();
        }
        drop(sender);
        writer.await.unwrap().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() <= MAX_RUN_LOG as usize);
        assert!(bytes.starts_with(LOG_TRUNCATED));
        assert!(bytes.ends_with(&vec![b'a' + (299 % 26) as u8; 8192]));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn active_run_slots_are_a_cross_process_ceiling() {
        let base = std::env::temp_dir().join(format!("elmee-run-slots-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let held: Vec<_> = (0..MAX_ACTIVE_RUNS).map(|_| claim_slot_in(&base).unwrap()).collect();
        assert!(claim_slot_in(&base).unwrap_err().to_string().contains("active runs"));
        drop(held);
        assert!(claim_slot_in(&base).is_ok(), "a completed run returns its slot");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn corrupt_run_slot_is_an_io_error_not_a_busy_slot() {
        let base = std::env::temp_dir().join(elmee_core::unique("run-slot-error"));
        fs::create_dir_all(base.join("slots/0")).unwrap();
        assert!(matches!(claim_slot_in(&base), Err(Error::Io(_))));
        assert!(!base.join("slots/1").exists(), "storage failure must not silently claim another slot");
        fs::remove_dir_all(base).unwrap();
    }
}
