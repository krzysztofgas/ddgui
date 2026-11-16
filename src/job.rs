use std::ffi::CString;
use std::fs::{ self, File, OpenOptions };
use std::io::{ ErrorKind, Read, Write };
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{ Path, PathBuf };
use std::process::{ Child, Command, Stdio };
use std::sync::atomic::{ AtomicBool, Ordering };
use std::sync::{ Arc, Mutex };
use std::thread;
use std::time::{ Duration, Instant, SystemTime, UNIX_EPOCH };

use crate::disks::Disk;

const CHUNK: usize = 4 << 20;
const SECTOR: u64 = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ImageToDisk,
    DiskToImage,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Cancelled,
    Failed(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum Phase {
    Authorizing,
    Copying,
    Finishing,
    Done(Outcome),
}

pub struct Status {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub started: Option<Instant>,
}

pub struct Job {
    pub status: Arc<Mutex<Status>>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn start(direction: Direction, image: PathBuf, disk: Disk) -> Result<Job, String> {
        let total = match direction {
            Direction::ImageToDisk => {
                let meta = fs
                    ::metadata(&image)
                    .map_err(|e| format!("Couldn't read {}: {e}", image.display()))?;
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", image.display()));
                }
                if meta.len() == 0 {
                    return Err("The image is empty".to_string());
                }
                if meta.len() > disk.size {
                    return Err(
                        format!(
                            "The image ({}) is larger than {} ({})",
                            crate::format::bytes(meta.len()),
                            disk.id,
                            crate::format::bytes(disk.size)
                        )
                    );
                }
                meta.len()
            }
            Direction::DiskToImage => disk.size,
        };
        let status = Arc::new(
            Mutex::new(Status {
                phase: Phase::Authorizing,
                done: 0,
                total,
                started: None,
            })
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = Context {
            direction,
            image,
            disk,
            status: status.clone(),
            cancel: cancel.clone(),
        };
        thread::Builder
            ::new()
            .name("dd".into())
            .spawn(move || {
                let outcome = ctx.run();
                ctx.set_phase(Phase::Done(outcome));
            })
            .map_err(|e| format!("Couldn't start worker: {e}"))?;
        Ok(Job { status, cancel })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

struct Context {
    direction: Direction,
    image: PathBuf,
    disk: Disk,
    status: Arc<Mutex<Status>>,
    cancel: Arc<AtomicBool>,
}

struct TempFifo {
    dir: PathBuf,
    path: PathBuf,
}

impl TempFifo {
    fn create() -> Result<TempFifo, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ddgui-{}-{nanos}", std::process::id()));
        fs::create_dir(&dir).map_err(|e| format!("Couldn't create temporary folder: {e}"))?;
        let path = dir.join("pipe");
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_|
            "Invalid temporary path".to_string()
        )?;
        if (unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }) != 0 {
            let err = std::io::Error::last_os_error();
            let _ = fs::remove_dir(&dir);
            return Err(format!("Couldn't create pipe: {err}"));
        }
        Ok(TempFifo { dir, path })
    }

    fn release_waiters(&self) {
        let _ = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path);
    }
}

impl Drop for TempFifo {
    fn drop(&mut self) {
        self.release_waiters();
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.dir);
    }
}

impl Context {
    fn set_phase(&self, phase: Phase) {
        if let Ok(mut s) = self.status.lock() {
            if phase == Phase::Copying && s.started.is_none() {
                s.started = Some(Instant::now());
            }
            s.phase = phase;
        }
    }

    fn add_done(&self, n: u64) {
        if let Ok(mut s) = self.status.lock() {
            s.done = (s.done + n).min(s.total);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn run(&self) -> Outcome {
        let fifo = match TempFifo::create() {
            Ok(f) => f,
            Err(e) => {
                return Outcome::Failed(e);
            }
        };
        let mut output = None;
        if self.direction == Direction::DiskToImage {
            match File::create(&self.image) {
                Ok(f) => {
                    output = Some(f);
                }
                Err(e) => {
                    return Outcome::Failed(
                        format!("Couldn't create {}: {e}", self.image.display())
                    );
                }
            }
        }
        let child = match self.spawn_dd(&fifo.path) {
            Ok(c) => Arc::new(Mutex::new(c)),
            Err(e) => {
                self.discard_output(output);
                return Outcome::Failed(e);
            }
        };
        let opened = Arc::new(AtomicBool::new(false));
        let aborted = Arc::new(AtomicBool::new(false));
        let watcher = self.spawn_watcher(&fifo, child.clone(), opened.clone(), aborted.clone());

        let pipe = match self.direction {
            Direction::ImageToDisk => OpenOptions::new().write(true).open(&fifo.path),
            Direction::DiskToImage => File::open(&fifo.path),
        };
        opened.store(true, Ordering::SeqCst);
        let _ = watcher.join();

        let copy_result = match pipe {
            Err(e) => Err(format!("Couldn't open pipe: {e}")),
            Ok(_) if aborted.load(Ordering::SeqCst) => Ok(()),
            Ok(pipe) => {
                self.set_phase(Phase::Copying);
                let result = match self.direction {
                    Direction::ImageToDisk => self.feed(pipe),
                    Direction::DiskToImage =>
                        match output.as_mut() {
                            Some(out) => self.drain(pipe, out),
                            None => Err("Missing output file".to_string()),
                        }
                };
                if !self.cancelled() {
                    self.set_phase(Phase::Finishing);
                }
                result
            }
        };

        let dd_result = finish(&child);
        let outcome = if self.cancelled() {
            Outcome::Cancelled
        } else {
            match (dd_result, copy_result) {
                (Err(e), _) => Outcome::Failed(e),
                (Ok(()), Err(e)) => Outcome::Failed(e),
                (Ok(()), Ok(())) if aborted.load(Ordering::SeqCst) => {
                    Outcome::Failed("dd exited before starting".to_string())
                }
                (Ok(()), Ok(())) => self.verify(),
            }
        };
        if outcome == Outcome::Finished {
            if let Some(Err(e)) = output.map(|out| out.sync_all()) {
                return Outcome::Failed(format!("Couldn't save image: {e}"));
            }
        } else {
            self.discard_output(output);
        }
        outcome
    }

    fn verify(&self) -> Outcome {
        let (done, total) = match self.status.lock() {
            Ok(s) => (s.done, s.total),
            Err(_) => {
                return Outcome::Failed("Internal state error".to_string());
            }
        };
        if done < total {
            Outcome::Failed(
                format!(
                    "Copy ended early after {} of {}",
                    crate::format::bytes(done),
                    crate::format::bytes(total)
                )
            )
        } else {
            Outcome::Finished
        }
    }

    fn discard_output(&self, output: Option<File>) {
        if let Some(out) = output {
            drop(out);
            let _ = fs::remove_file(&self.image);
        }
    }

    fn spawn_dd(&self, fifo: &Path) -> Result<Child, String> {
        let pipe = shell_quote(&fifo.to_string_lossy());
        let node = shell_quote(&self.disk.node());
        let raw = shell_quote(&self.disk.raw_node());
        let script = match self.direction {
            Direction::ImageToDisk =>
                format!(
                    "/usr/sbin/diskutil unmountDisk force {node} >/dev/null && /bin/dd if={pipe} of={raw} ibs=4m obs=4m && /bin/sync"
                ),
            Direction::DiskToImage =>
                format!(
                    "/usr/sbin/diskutil unmountDisk {node} >/dev/null && /bin/dd if={raw} of={pipe} ibs=4m obs=4m"
                ),
        };
        Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "do shell script (item 1 of argv) with administrator privileges",
                "-e",
                "end run",
                &script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Couldn't start dd: {e}"))
    }

    fn spawn_watcher(
        &self,
        fifo: &TempFifo,
        child: Arc<Mutex<Child>>,
        opened: Arc<AtomicBool>,
        aborted: Arc<AtomicBool>
    ) -> thread::JoinHandle<()> {
        let cancel = self.cancel.clone();
        let path = fifo.path.clone();
        thread::spawn(move || {
            let mut killed = false;
            while !opened.load(Ordering::SeqCst) {
                let exited = match child.lock() {
                    Ok(mut c) => {
                        if cancel.load(Ordering::SeqCst) && !killed {
                            let _ = c.kill();
                            killed = true;
                        }
                        !matches!(c.try_wait(), Ok(None))
                    }
                    Err(_) => true,
                };
                if exited {
                    aborted.store(true, Ordering::SeqCst);
                    while !opened.load(Ordering::SeqCst) {
                        let _ = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .custom_flags(libc::O_NONBLOCK)
                            .open(&path);
                        thread::sleep(Duration::from_millis(20));
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    }

    fn feed(&self, mut pipe: File) -> Result<(), String> {
        let mut input = File::open(&self.image).map_err(|e|
            format!("Couldn't open {}: {e}", self.image.display())
        )?;
        let mut buf = vec![0u8; CHUNK];
        let mut written: u64 = 0;
        loop {
            if self.cancelled() {
                return Ok(());
            }
            let n = match input.read(&mut buf) {
                Ok(0) => {
                    break;
                }
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    continue;
                }
                Err(e) => {
                    return Err(format!("Couldn't read image: {e}"));
                }
            };
            write_pipe(&mut pipe, &buf[..n])?;
            written += n as u64;
            self.add_done(n as u64);
        }
        let pad = (SECTOR - (written % SECTOR)) % SECTOR;
        if pad > 0 && written + pad <= self.disk.size {
            write_pipe(&mut pipe, &vec![0u8; pad as usize])?;
        }
        Ok(())
    }

    fn drain(&self, mut pipe: File, out: &mut File) -> Result<(), String> {
        let mut buf = vec![0u8; CHUNK];
        loop {
            if self.cancelled() {
                return Ok(());
            }
            let n = match pipe.read(&mut buf) {
                Ok(0) => {
                    return Ok(());
                }
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    continue;
                }
                Err(e) => {
                    return Err(format!("Couldn't read from dd: {e}"));
                }
            };
            out.write_all(&buf[..n]).map_err(|e| format!("Couldn't write image: {e}"))?;
            self.add_done(n as u64);
        }
    }
}

fn write_pipe(pipe: &mut File, data: &[u8]) -> Result<(), String> {
    pipe.write_all(data).map_err(|e| {
        if e.kind() == ErrorKind::BrokenPipe {
            "dd stopped unexpectedly".to_string()
        } else {
            format!("Couldn't send data to dd: {e}")
        }
    })
}

fn finish(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let mut guard = child.lock().map_err(|_| "Internal state error".to_string())?;
    let mut stderr = String::new();
    if let Some(mut pipe) = guard.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = guard.wait().map_err(|e| format!("Couldn't wait for dd: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(osascript_error(&stderr))
    }
}

fn osascript_error(stderr: &str) -> String {
    let text = stderr.trim();
    if text.ends_with("(-128)") {
        return "Authorization was cancelled".to_string();
    }
    let text = text
        .split_once("execution error: ")
        .map(|(_, rest)| rest)
        .unwrap_or(text);
    let text = match text.rfind(" (") {
        Some(i) if text.ends_with(')') => &text[..i],
        _ => text,
    };
    let text = text
        .split(['\r', '\n'])
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if text.is_empty() {
        "dd failed".to_string()
    } else {
        text.to_string()
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
