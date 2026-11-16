use std::ffi::CString;
use std::fs::{ self, DirBuilder, File, OpenOptions };
use std::io::{ ErrorKind, Read, Write };
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{ DirBuilderExt, OpenOptionsExt };
use std::panic::{ self, AssertUnwindSafe };
use std::path::{ Path, PathBuf };
use std::process::{ Child, Command, Stdio };
use std::sync::atomic::{ AtomicBool, AtomicU64, Ordering };
use std::sync::{ Arc, Mutex, OnceLock, PoisonError };
use std::thread::{ self, JoinHandle };
use std::time::{ Duration, Instant };

use crate::disks::{ self, Disk };
use crate::error::{ Context, Error, Result, bail, lock };
use crate::format;
use crate::formats::{ self, Counting, Source, Target, ZipEntry };
use crate::pump::{ PumpError, pump };

const SECTOR: u64 = 512;
const DEFAULT_BLOCK: &str = "4m";
const PROGRESS_SCALE: u64 = 10_000;
const OPERANDS: [&str; 16] = [
    "bs",
    "cbs",
    "conv",
    "count",
    "files",
    "fillchar",
    "ibs",
    "iflag",
    "iseek",
    "obs",
    "oflag",
    "oseek",
    "seek",
    "skip",
    "speed",
    "status",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args(Vec<(String, String)>);

impl Args {
    pub fn parse(text: &str) -> Result<Self> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for token in text.split_whitespace() {
            let Some((key, value)) = token.split_once('=') else {
                bail!("“{token}” is not a dd operand (expected key=value)");
            };
            if matches!(key, "if" | "of") {
                bail!("“{key}=” is set automatically");
            }
            if !OPERANDS.contains(&key) {
                bail!("Unknown dd operand “{key}”");
            }
            if value.is_empty() {
                bail!("“{key}=” needs a value");
            }
            if value.chars().any(char::is_control) {
                bail!("“{key}=” contains invalid characters");
            }
            if pairs.iter().any(|(k, _)| k == key) {
                bail!("“{key}=” is given more than once");
            }
            pairs.push((key.to_owned(), value.to_owned()));
        }
        Ok(Self(pairs))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    fn operands(&self) -> String {
        let mut out = Vec::new();
        if !self.has("bs") && !self.has("ibs") {
            out.push(format!("ibs={DEFAULT_BLOCK}"));
        }
        if !self.has("bs") && !self.has("obs") {
            out.push(format!("obs={DEFAULT_BLOCK}"));
        }
        out.extend(self.0.iter().map(|(k, v)| shell_quote(&format!("{k}={v}"))));
        out.join(" ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ImageToDisk,
    DiskToImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Authorizing,
    Copying,
    Finishing,
    Done(Outcome),
}

#[derive(Clone, Debug)]
enum Layout {
    Write {
        source: Source,
        zip: Option<ZipEntry>,
    },
    Read {
        target: Target,
    },
}

pub struct Plan {
    layout: Layout,
    image: PathBuf,
    disk: Disk,
    args: Args,
    total: u64,
}

impl Plan {
    pub fn prepare(
        direction: Direction,
        image: PathBuf,
        selected: &Disk,
        args: Args
    ) -> Result<Self> {
        let disk = disks::lookup(&selected.id)?;
        if disk.name != selected.name || disk.size != selected.size {
            bail!("{} changed since it was selected. Check the device and try again.", disk.id);
        }
        let (layout, total) = match direction {
            Direction::ImageToDisk => check_source(&image, &disk)?,
            Direction::DiskToImage => check_destination(&image, &disk, &args)?,
        };
        Ok(Self { layout, image, disk, args, total })
    }

    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    pub fn image(&self) -> &Path {
        &self.image
    }

    pub fn description(&self) -> &'static str {
        match self.layout {
            Layout::Write { source, .. } => source.label(),
            Layout::Read { target } => target.label(),
        }
    }

    fn exact(&self) -> bool {
        self.args.is_empty() &&
            (match &self.layout {
                Layout::Write { source, .. } => !source.is_stream(),
                Layout::Read { .. } => true,
            })
    }
}

fn check_source(image: &Path, disk: &Disk) -> Result<(Layout, u64)> {
    let source = Source::detect(image)?;
    if disks::disks_for_path(image)?.contains(&disk.id) {
        bail!("The image is stored on {}, the device it would overwrite", disk.id);
    }
    let too_big = |size: u64| -> Result<()> {
        if size > disk.size {
            bail!(
                "The image ({}) is larger than {} ({})",
                format::bytes(size),
                disk.id,
                format::bytes(disk.size)
            );
        }
        Ok(())
    };
    let mut zip = None;
    let total = match source {
        Source::Folder => 0,
        Source::Dmg => {
            let size = formats::dmg_size(image)?;
            too_big(size)?;
            ensure_free(&std::env::temp_dir(), size)?;
            size
        }
        Source::Zip => {
            let entry = formats::zip_entry(image)?;
            too_big(entry.size)?;
            let size = entry.size;
            zip = Some(entry);
            size
        }
        _ => {
            let len = fs::metadata(image).context("Couldn't read the image")?.len();
            if len == 0 {
                bail!("The image is empty");
            }
            if source == Source::Raw {
                too_big(len)?;
            }
            File::open(image).context(format!("Couldn't open {}", image.display()))?;
            len
        }
    };
    Ok((Layout::Write { source, zip }, total))
}

fn check_destination(image: &Path, disk: &Disk, args: &Args) -> Result<(Layout, u64)> {
    let parent = image
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::new("Choose a folder to save the image in"))?;
    if !parent.is_dir() {
        bail!("{} is not a folder", parent.display());
    }
    if image.is_dir() {
        bail!("{} is a folder", image.display());
    }
    if disks::disks_for_path(parent)?.contains(&disk.id) {
        bail!("The image can't be saved on {}, the device being read", disk.id);
    }
    let target = Target::from_path(image);
    if args.is_empty() && target == Target::Raw {
        ensure_free(parent, disk.size)?;
    }
    Ok((Layout::Read { target }, disk.size))
}

fn ensure_free(dir: &Path, needed: u64) -> Result<()> {
    let free = free_space(dir)?;
    if free < needed {
        bail!(
            "Not enough free space on {}: {} needed, {} available",
            dir.display(),
            format::bytes(needed),
            format::bytes(free)
        );
    }
    Ok(())
}

pub struct Snapshot {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub written: u64,
    pub elapsed: Option<Duration>,
    pub cancelled: bool,
    pub stream: bool,
}

struct Shared {
    phase: Mutex<Phase>,
    done: AtomicU64,
    total: AtomicU64,
    written: AtomicU64,
    started: OnceLock<Instant>,
    cancel: AtomicBool,
    stream: bool,
}

impl Shared {
    fn set_phase(&self, phase: Phase) {
        if phase == Phase::Copying {
            let _ = self.started.set(Instant::now());
        }
        *lock(&self.phase) = phase;
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn reset(&self, total: u64) {
        self.done.store(0, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
}

pub struct Job {
    shared: Arc<Shared>,
}

impl Job {
    pub fn start(plan: Plan) -> Result<Self> {
        let stream = matches!(&plan.layout, Layout::Write { source, .. } if source.is_stream());
        let preparing =
            matches!(&plan.layout, Layout::Write { source, .. } if source.needs_preparing());
        let shared = Arc::new(Shared {
            phase: Mutex::new(if preparing { Phase::Preparing } else { Phase::Authorizing }),
            done: AtomicU64::new(0),
            total: AtomicU64::new(plan.total),
            written: AtomicU64::new(0),
            started: OnceLock::new(),
            cancel: AtomicBool::new(false),
            stream,
        });
        let worker = Worker { plan, shared: shared.clone() };
        thread::Builder
            ::new()
            .name("dd".into())
            .spawn(move || {
                let outcome = panic
                    ::catch_unwind(AssertUnwindSafe(|| worker.run()))
                    .unwrap_or_else(|_| Outcome::Failed("Internal error".to_owned()));
                worker.shared.set_phase(Phase::Done(outcome));
            })
            .context("Couldn't start worker")?;
        Ok(Self { shared })
    }

    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: lock(&self.shared.phase).clone(),
            done: self.shared.done.load(Ordering::Relaxed),
            total: self.shared.total.load(Ordering::Relaxed),
            written: self.shared.written.load(Ordering::Relaxed),
            elapsed: self.shared.started.get().map(Instant::elapsed),
            cancelled: self.shared.cancelled(),
            stream: self.shared.stream,
        }
    }

    pub fn wait(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if matches!(*lock(&self.shared.phase), Phase::Done(_)) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

enum CopyError {
    Stopped,
    Other(Error),
}

impl From<Error> for CopyError {
    fn from(e: Error) -> Self {
        Self::Other(e)
    }
}

struct Worker {
    plan: Plan,
    shared: Arc<Shared>,
}

impl Worker {
    fn run(&self) -> Outcome {
        let _awake = KeepAwake::start();
        let result = self.execute();
        if self.shared.cancelled() {
            return Outcome::Cancelled;
        }
        match result {
            Ok(()) => Outcome::Finished,
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }

    fn execute(&self) -> Result<()> {
        let staging = self.stage()?;
        if self.shared.cancelled() {
            return Ok(());
        }
        let (image, layout) = match &staging {
            Some(staged) =>
                (staged.path.as_path(), Layout::Write { source: Source::Raw, zip: None }),
            None => (self.plan.image.as_path(), self.plan.layout.clone()),
        };
        self.shared.set_phase(Phase::Authorizing);

        let fifo = TempFifo::create()?;
        let mut output = match layout {
            Layout::Read { .. } => Some(PartialFile::create(&self.plan.image)?),
            Layout::Write { .. } => None,
        };
        let dd = Dd::spawn(&self.script(fifo.path()))?;

        let copied = match self.connect(&fifo, &dd) {
            Ok(Some(pipe)) => {
                self.shared.set_phase(Phase::Copying);
                let result = match (&layout, output.as_mut()) {
                    (Layout::Write { source, zip }, _) =>
                        self.feed(pipe, image, *source, zip.as_ref()),
                    (Layout::Read { target }, Some(out)) => self.drain(pipe, out.file(), *target),
                    (Layout::Read { .. }, None) =>
                        Err(CopyError::Other(Error::new("Missing output file"))),
                };
                if !self.shared.cancelled() {
                    self.shared.set_phase(Phase::Finishing);
                }
                Some(result)
            }
            Ok(None) => None,
            Err(e) => Some(Err(CopyError::Other(e))),
        };

        dd.finish()?;
        if self.shared.cancelled() {
            return Ok(());
        }
        let exact = self.plan.exact();
        match copied {
            None => bail!("dd exited before starting"),
            Some(Err(CopyError::Other(e))) => {
                return Err(e);
            }
            Some(Err(CopyError::Stopped)) if self.plan.args.is_empty() => {
                bail!("dd stopped before the whole image was written");
            }
            Some(_) => {}
        }
        if exact {
            self.verify()?;
        }
        if let Some(out) = output {
            out.commit()?;
        }
        Ok(())
    }

    fn stage(&self) -> Result<Option<Staged>> {
        let Layout::Write { source, .. } = &self.plan.layout else {
            return Ok(None);
        };
        if !source.needs_preparing() {
            return Ok(None);
        }
        let staged = Staged::new(if *source == Source::Folder { "iso" } else { "cdr" })?;
        let mut command = match source {
            Source::Dmg => formats::convert_dmg(&self.plan.image, &staged.path),
            _ => {
                let estimate = formats::folder_size(&self.plan.image)?;
                let needed = estimate + estimate / 10 + (16 << 20);
                if estimate > self.plan.disk.size {
                    bail!(
                        "The folder ({}) is larger than {} ({})",
                        format::bytes(estimate),
                        self.plan.disk.id,
                        format::bytes(self.plan.disk.size)
                    );
                }
                ensure_free(&staged.dir, needed)?;
                formats::make_iso(&self.plan.image, &staged.path)
            }
        };
        self.shared.reset(PROGRESS_SCALE);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Couldn't run hdiutil")?;
        let shared = self.shared.clone();
        let out = child.stdout.take().map(|s| {
            formats::read_lines(s, move |line| {
                if let Some(p) = formats::parse_percent(line) {
                    shared.done.store((p * (PROGRESS_SCALE as f64)) as u64, Ordering::Relaxed);
                }
            })
        });
        let err = child.stderr.take().map(|s| formats::read_lines(s, |_| {}));
        let status = loop {
            if self.shared.cancelled() {
                let _ = child.kill();
            }
            if let Some(status) = child.try_wait().context("Couldn't wait for hdiutil")? {
                break status;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let out = out.and_then(|h| h.join().ok()).unwrap_or_default();
        let err = err.and_then(|h| h.join().ok()).unwrap_or_default();
        if self.shared.cancelled() {
            return Ok(Some(staged));
        }
        if !status.success() {
            let message = err
                .lines()
                .chain(out.lines().filter_map(|l| l.strip_prefix("ERROR:")))
                .map(|l| l.trim().trim_start_matches("hdiutil:").trim())
                .find(|l| !l.is_empty())
                .unwrap_or("hdiutil failed")
                .to_owned();
            bail!("Couldn't prepare the image: {message}");
        }
        let size = fs::metadata(&staged.path).context("Couldn't read the prepared image")?.len();
        if size == 0 {
            bail!("The prepared image is empty");
        }
        if size > self.plan.disk.size {
            bail!(
                "The prepared image ({}) is larger than {} ({})",
                format::bytes(size),
                self.plan.disk.id,
                format::bytes(self.plan.disk.size)
            );
        }
        self.shared.reset(size);
        Ok(Some(staged))
    }

    fn script(&self, fifo: &Path) -> String {
        let pipe = shell_quote(&fifo.to_string_lossy());
        let node = shell_quote(&self.plan.disk.node());
        let raw = shell_quote(&self.plan.disk.raw_node());
        let operands = self.plan.args.operands();
        match self.plan.layout {
            Layout::Write { .. } =>
                format!(
                    "/usr/sbin/diskutil unmountDisk force {node} >/dev/null && /bin/dd if={pipe} of={raw} {operands} && /bin/sync"
                ),
            Layout::Read { .. } =>
                format!(
                    "/usr/sbin/diskutil unmountDisk {node} >/dev/null && /bin/dd if={raw} of={pipe} {operands}"
                ),
        }
    }

    fn connect(&self, fifo: &TempFifo, dd: &Dd) -> Result<Option<File>> {
        let opened = AtomicBool::new(false);
        let aborted = AtomicBool::new(false);
        let writing = matches!(self.plan.layout, Layout::Write { .. });
        let pipe = thread::scope(|scope| {
            scope.spawn(|| {
                let mut killed = false;
                while !opened.load(Ordering::SeqCst) {
                    if self.shared.cancelled() && !killed {
                        dd.kill();
                        killed = true;
                    }
                    if dd.exited() {
                        aborted.store(true, Ordering::SeqCst);
                        while !opened.load(Ordering::SeqCst) {
                            fifo.release_waiters();
                            thread::sleep(Duration::from_millis(20));
                        }
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            });
            let pipe = if writing {
                OpenOptions::new().write(true).open(fifo.path())
            } else {
                File::open(fifo.path())
            };
            opened.store(true, Ordering::SeqCst);
            pipe
        });
        let pipe = pipe.context("Couldn't open pipe")?;
        Ok((!aborted.load(Ordering::SeqCst)).then_some(pipe))
    }

    fn feed(
        &self,
        mut pipe: File,
        image: &Path,
        source: Source,
        zip: Option<&ZipEntry>
    ) -> Result<(), CopyError> {
        let written = match zip {
            Some(entry) => self.feed_zip(&mut pipe, image, entry)?,
            None => {
                let input = File::open(image).context(
                    format!("Couldn't open {}", image.display())
                )?;
                no_cache(&input);
                let counted = Counting::new(input, &self.shared.done);
                let reader = formats::decoder(source, counted)?;
                self.send(reader, &mut pipe)?
            }
        };
        if self.shared.cancelled() {
            return Ok(());
        }
        let pad = (SECTOR - (written % SECTOR)) % SECTOR;
        if pad > 0 && written + pad <= self.plan.disk.size {
            pipe.write_all(&vec![0; pad as usize]).map_err(pipe_error)?;
        }
        Ok(())
    }

    fn feed_zip(
        &self,
        pipe: &mut File,
        archive: &Path,
        entry: &ZipEntry
    ) -> Result<u64, CopyError> {
        let mut child = formats
            ::unzip_command(archive, entry)
            .spawn()
            .context("Couldn't run unzip")?;
        let err = child.stderr.take().map(|s| formats::read_lines(s, |_| {}));
        let stdout = child.stdout.take().ok_or_else(|| Error::new("Couldn't read from unzip"))?;
        let result = self.send(Counting::new(stdout, &self.shared.done), pipe);
        if result.is_err() || self.shared.cancelled() {
            let _ = child.kill();
        }
        let status = child.wait().context("Couldn't wait for unzip")?;
        let err = err.and_then(|h| h.join().ok()).unwrap_or_default();
        let written = result?;
        if !status.success() && !self.shared.cancelled() {
            let message = err
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("unzip failed");
            bail_copy(format!("Couldn't extract the image: {message}"))?;
        }
        Ok(written)
    }

    fn send(&self, reader: impl Read + Send, pipe: &mut File) -> Result<u64, CopyError> {
        pump(reader, pipe, &self.shared.cancel, |n| {
            self.shared.written.fetch_add(n, Ordering::Relaxed);
        }).map_err(|e| {
            match e {
                PumpError::Read(e) => Error::new(format!("Couldn't read the image: {e}")).into(),
                PumpError::Write(e) => pipe_error(e),
            }
        })
    }

    fn drain(&self, pipe: File, out: &mut File, target: Target) -> Result<(), CopyError> {
        let mut encoder = target.encoder(out)?;
        pump(pipe, &mut encoder, &self.shared.cancel, |n| {
            self.shared.done.fetch_add(n, Ordering::Relaxed);
            self.shared.written.fetch_add(n, Ordering::Relaxed);
        }).map_err(|e| {
            match e {
                PumpError::Read(e) => Error::new(format!("Couldn't read from dd: {e}")),
                PumpError::Write(e) => Error::new(format!("Couldn't write the image: {e}")),
            }
        })?;
        if !self.shared.cancelled() {
            encoder.finish().context("Couldn't finish the image")?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        let done = self.shared.done.load(Ordering::Relaxed);
        let total = self.shared.total.load(Ordering::Relaxed);
        if done < total {
            bail!("Copy ended early after {} of {}", format::bytes(done), format::bytes(total));
        }
        Ok(())
    }
}

fn bail_copy(message: String) -> Result<(), CopyError> {
    Err(CopyError::Other(Error::new(message)))
}

fn pipe_error(e: std::io::Error) -> CopyError {
    if e.kind() == ErrorKind::BrokenPipe {
        CopyError::Stopped
    } else {
        CopyError::Other(Error::new(format!("Couldn't send data to dd: {e}")))
    }
}

struct Dd {
    child: Mutex<Child>,
    stderr: Mutex<Option<JoinHandle<String>>>,
}

impl Dd {
    fn spawn(script: &str) -> Result<Self> {
        let mut child = Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "do shell script (item 1 of argv) with administrator privileges",
                "-e",
                "end run",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Couldn't start dd")?;
        let stderr = child.stderr.take().map(|pipe| formats::read_lines(pipe, |_| {}));
        Ok(Self { child: Mutex::new(child), stderr: Mutex::new(stderr) })
    }

    fn kill(&self) {
        let _ = lock(&self.child).kill();
    }

    fn exited(&self) -> bool {
        !matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn finish(&self) -> Result<()> {
        let status = lock(&self.child).wait().context("Couldn't wait for dd")?;
        let stderr = lock(&self.stderr)
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        if status.success() {
            Ok(())
        } else {
            Err(Error::new(osascript_error(&stderr)))
        }
    }
}

impl Drop for Dd {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(PoisonError::into_inner);
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn private_dir(label: &str) -> Result<PathBuf> {
    let token = unsafe { (u64::from(libc::arc4random()) << 32) | u64::from(libc::arc4random()) };
    let dir = std::env
        ::temp_dir()
        .join(format!("ddgui-{label}-{}-{token:016x}", std::process::id()));
    DirBuilder::new().mode(0o700).create(&dir).context("Couldn't create temporary folder")?;
    Ok(dir)
}

struct TempFifo {
    dir: PathBuf,
    path: PathBuf,
}

impl TempFifo {
    fn create() -> Result<Self> {
        let dir = private_dir("pipe")?;
        let path = dir.join("pipe");
        let created = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new("Invalid temporary path"))
            .and_then(|c_path| {
                if (unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error()).context("Couldn't create pipe")
                }
            });
        if let Err(e) = created {
            let _ = fs::remove_dir(&dir);
            return Err(e);
        }
        Ok(Self { dir, path })
    }

    fn path(&self) -> &Path {
        &self.path
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

struct Staged {
    dir: PathBuf,
    path: PathBuf,
}

impl Staged {
    fn new(extension: &str) -> Result<Self> {
        let dir = private_dir("stage")?;
        let path = dir.join(format!("image.{extension}"));
        Ok(Self { dir, path })
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct PartialFile {
    file: File,
    temp: PathBuf,
    dest: PathBuf,
    committed: bool,
}

impl PartialFile {
    fn create(dest: &Path) -> Result<Self> {
        let name = dest
            .file_name()
            .ok_or_else(|| Error::new("Invalid image name"))?
            .to_string_lossy();
        let temp = dest.with_file_name(format!(".{name}.ddgui-partial"));
        let _ = fs::remove_file(&temp);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temp)
            .context(format!("Couldn't create {}", dest.display()))?;
        no_cache(&file);
        Ok(Self { file, temp, dest: dest.to_owned(), committed: false })
    }

    fn file(&mut self) -> &mut File {
        &mut self.file
    }

    fn commit(mut self) -> Result<()> {
        self.file.sync_all().context("Couldn't save the image")?;
        fs
            ::rename(&self.temp, &self.dest)
            .context(format!("Couldn't save {}", self.dest.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

struct KeepAwake(Option<Child>);

impl KeepAwake {
    fn start() -> Self {
        let pid = std::process::id().to_string();
        Self(
            Command::new("/usr/bin/caffeinate")
                .args(["-ims", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        )
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn no_cache(file: &File) {
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

fn free_space(path: &Path) -> Result<u64> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_|
        Error::new(format!("Invalid path {}", path.display()))
    )?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if (unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) }) != 0 {
        return Err(std::io::Error::last_os_error()).context(
            format!("Couldn't check free space on {}", path.display())
        );
    }
    let stat = unsafe { stat.assume_init() };
    Ok(u64::from(stat.f_bavail).saturating_mul(stat.f_frsize))
}

fn osascript_error(stderr: &str) -> String {
    let text = stderr.trim();
    if text.ends_with("(-128)") {
        return "Authorization was cancelled".to_owned();
    }
    let text = text.split_once("execution error: ").map_or(text, |(_, rest)| rest);
    let text = match text.rfind(" (") {
        Some(i) if text.ends_with(')') => &text[..i],
        _ => text,
    };
    text.split(['\r', '\n'])
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("dd failed")
        .to_owned()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operands() {
        let args = Args::parse("  bs=1m   conv=sync ").unwrap();
        assert_eq!(args.operands(), "'bs=1m' 'conv=sync'");
        assert!(Args::parse("").unwrap().is_empty());
    }

    #[test]
    fn keeps_default_blocks_unless_overridden() {
        assert_eq!(Args::default().operands(), "ibs=4m obs=4m");
        assert_eq!(Args::parse("obs=1m").unwrap().operands(), "ibs=4m 'obs=1m'");
    }

    #[test]
    fn rejects_bad_operands() {
        for bad in ["of=/dev/disk0", "if=x", "rm", "foo=1", "bs=", "bs=1 bs=2"] {
            assert!(Args::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote("$(x)"), "'$(x)'");
    }

    #[test]
    fn extracts_osascript_errors() {
        assert_eq!(
            osascript_error("0:1: execution error: User canceled. (-128)\n"),
            "Authorization was cancelled"
        );
        assert_eq!(
            osascript_error(
                "7:45: execution error: dd: /dev/rdisk9: Device busy\r0+0 records in (1)"
            ),
            "dd: /dev/rdisk9: Device busy"
        );
        assert_eq!(osascript_error(""), "dd failed");
    }

    #[test]
    fn temp_fifo_cleans_up() {
        let fifo = TempFifo::create().unwrap();
        let dir = fifo.dir.clone();
        assert!(fifo.path().exists());
        drop(fifo);
        assert!(!dir.exists());
    }

    #[test]
    fn partial_file_is_removed_unless_committed() {
        let dir = std::env::temp_dir().join(format!("ddgui-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("a.img");
        let mut partial = PartialFile::create(&dest).unwrap();
        partial.file().write_all(b"x").unwrap();
        let temp = partial.temp.clone();
        drop(partial);
        assert!(!temp.exists() && !dest.exists());
        let mut partial = PartialFile::create(&dest).unwrap();
        partial.file().write_all(b"y").unwrap();
        partial.commit().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"y");
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    fn worker(image: PathBuf, source: Source) -> Worker {
        let disk = Disk { id: "disk999".into(), name: "Test".into(), size: 1 << 40 };
        let plan = Plan {
            layout: Layout::Write { source, zip: None },
            image,
            disk,
            args: Args::default(),
            total: 0,
        };
        let shared = Arc::new(Shared {
            phase: Mutex::new(Phase::Preparing),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            written: AtomicU64::new(0),
            started: OnceLock::new(),
            cancel: AtomicBool::new(false),
            stream: false,
        });
        Worker { plan, shared }
    }

    #[test]
    fn stages_folders_and_dmgs() {
        let dir = private_dir("stage-test").unwrap();
        let folder = dir.join("Files");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("sub/a.txt"), vec![1u8; 100_000]).unwrap();
        let staged = worker(folder.clone(), Source::Folder).stage().unwrap().unwrap();
        let iso = fs::metadata(&staged.path).unwrap().len();
        assert!(iso > 100_000);

        let dmg = dir.join("t.dmg");
        let ok = Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-format", "UDZO", "-srcfolder"])
            .arg(&folder)
            .arg(&dmg)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(Source::detect(&dmg).unwrap(), Source::Dmg);
        let w = worker(dmg.clone(), Source::Dmg);
        let staged = w.stage().unwrap().unwrap();
        let raw = fs::metadata(&staged.path).unwrap().len();
        assert_eq!(raw, formats::dmg_size(&dmg).unwrap());
        assert_eq!(w.shared.total.load(Ordering::Relaxed), raw);
        let staged_dir = staged.dir.clone();
        drop(staged);
        assert!(!staged_dir.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
