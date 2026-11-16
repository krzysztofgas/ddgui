use std::fs::File;
use std::io::{ self, BufRead, BufReader, Read, Seek, SeekFrom, Write };
use std::path::Path;
use std::process::{ Command, Stdio };
use std::sync::atomic::{ AtomicU64, Ordering };
use std::thread;

use plist::Value;

use crate::error::{ Context, Error, Result, bail };

const HDIUTIL: &str = "/usr/bin/hdiutil";
const UNZIP: &str = "/usr/bin/unzip";
const IMAGE_EXTENSIONS: [&str; 6] = ["img", "iso", "raw", "bin", "dd", "dmg"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Raw,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Zip,
    Dmg,
    Folder,
}

impl Source {
    pub fn detect(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).context(format!("Couldn't read {}", path.display()))?;
        if meta.is_dir() {
            return Ok(Self::Folder);
        }
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let mut file = File::open(path).context(format!("Couldn't open {}", path.display()))?;
        let mut head = [0u8; 6];
        let read = read_up_to(&mut file, &mut head).context("Couldn't read the image")?;
        let head = &head[..read];
        let kind = match head {
            [0x1f, 0x8b, ..] => Self::Gzip,
            [b'B', b'Z', b'h', d, ..] if d.is_ascii_digit() => Self::Bzip2,
            [0xfd, b'7', b'z', b'X', b'Z', 0x00] => Self::Xz,
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Self::Zstd,
            [b'P', b'K', 0x03, 0x04, ..] => Self::Zip,
            _ if has_udif_trailer(&mut file, meta.len())? => Self::Dmg,
            _ => Self::Raw,
        };
        Ok(kind)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => "raw image",
            Self::Gzip => "gzip-compressed image",
            Self::Bzip2 => "bzip2-compressed image",
            Self::Xz => "xz-compressed image",
            Self::Zstd => "zstd-compressed image",
            Self::Zip => "zip archive",
            Self::Dmg => "Apple disk image",
            Self::Folder => "folder, written as an ISO image",
        }
    }

    pub fn is_stream(self) -> bool {
        matches!(self, Self::Gzip | Self::Bzip2 | Self::Xz | Self::Zstd)
    }

    pub fn needs_preparing(self) -> bool {
        matches!(self, Self::Dmg | Self::Folder)
    }
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => {
                break;
            }
            Ok(n) => {
                filled += n;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(e);
            }
        }
    }
    Ok(filled)
}

fn has_udif_trailer(file: &mut File, len: u64) -> Result<bool> {
    if len < 512 {
        return Ok(false);
    }
    let mut magic = [0u8; 4];
    file
        .seek(SeekFrom::Start(len - 512))
        .and_then(|_| file.read_exact(&mut magic))
        .context("Couldn't read the image")?;
    Ok(&magic == b"koly")
}

pub fn decoder<'a, R: Read + Send + 'a>(
    kind: Source,
    input: R
) -> Result<Box<dyn Read + Send + 'a>> {
    Ok(match kind {
        Source::Gzip => Box::new(flate2::read::MultiGzDecoder::new(input)),
        Source::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(input)),
        Source::Xz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(input)),
        Source::Zstd =>
            Box::new(
                zstd::stream::read::Decoder::new(input).context("Couldn't start zstd decoder")?
            ),
        _ => Box::new(input),
    })
}

pub struct Counting<'a, R> {
    inner: R,
    count: &'a AtomicU64,
}

impl<'a, R> Counting<'a, R> {
    pub fn new(inner: R, count: &'a AtomicU64) -> Self {
        Self { inner, count }
    }
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub size: u64,
}

pub fn zip_entry(archive: &Path) -> Result<ZipEntry> {
    let output = Command::new(UNZIP)
        .arg("-l")
        .arg(archive)
        .stdin(Stdio::null())
        .output()
        .context("Couldn't run unzip")?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("Couldn't read the archive{}", if msg.is_empty() {
            String::new()
        } else {
            format!(": {msg}")
        });
    }
    pick_entry(parse_listing(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_listing(listing: &str) -> Vec<ZipEntry> {
    listing
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("---"))
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("---"))
        .filter_map(|line| {
            let (size, rest) = field(line)?;
            let (_date, rest) = field(rest)?;
            let (_time, rest) = field(rest)?;
            let name = rest.trim_start().to_owned();
            Some(ZipEntry { name, size: size.parse().ok()? })
        })
        .filter(|e| {
            !e.name.ends_with('/') &&
                !e.name.starts_with("__MACOSX/") &&
                !e.name
                    .rsplit('/')
                    .next()
                    .is_some_and(|n| n.starts_with('.'))
        })
        .collect()
}

fn field(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace)?;
    Some((&text[..end], &text[end..]))
}

fn pick_entry(entries: Vec<ZipEntry>) -> Result<ZipEntry> {
    if let [entry] = entries.as_slice() {
        return Ok(entry.clone());
    }
    let mut images = entries.into_iter().filter(|e| {
        Path::new(&e.name)
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| IMAGE_EXTENSIONS.contains(&x.to_ascii_lowercase().as_str()))
    });
    match (images.next(), images.next()) {
        (Some(entry), None) => Ok(entry),
        (None, _) => bail!("The archive doesn't contain a disk image"),
        (Some(_), Some(_)) =>
            bail!("The archive contains several images. Extract the one you want first."),
    }
}

pub fn zip_pattern(name: &str) -> String {
    name.chars()
        .map(|c| {
            match c {
                '*' | '?' | '[' | '\\' => format!("[{c}]"),
                c => c.to_string(),
            }
        })
        .collect()
}

pub fn unzip_command(archive: &Path, entry: &ZipEntry) -> Command {
    let mut command = Command::new(UNZIP);
    command
        .arg("-p")
        .arg(archive)
        .arg(zip_pattern(&entry.name))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub fn dmg_size(path: &Path) -> Result<u64> {
    let output = Command::new(HDIUTIL)
        .args(["imageinfo", "-plist"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("Couldn't run hdiutil")?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("Couldn't read the disk image{}", if msg.is_empty() {
            String::new()
        } else {
            format!(": {msg}")
        });
    }
    Value::from_reader_xml(output.stdout.as_slice())
        .context("Couldn't parse hdiutil output")?
        .as_dictionary()
        .and_then(|d| d.get("Size Information"))
        .and_then(Value::as_dictionary)
        .and_then(|d| d.get("Total Bytes"))
        .and_then(Value::as_unsigned_integer)
        .ok_or_else(|| Error::new("Couldn't determine the disk image size"))
}

pub fn convert_dmg(source: &Path, dest: &Path) -> Command {
    let mut command = Command::new(HDIUTIL);
    command.arg("convert").arg(source).args(["-format", "UDTO", "-puppetstrings", "-o"]).arg(dest);
    command
}

pub fn make_iso(folder: &Path, dest: &Path) -> Command {
    let name: String = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_owned())
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    let mut command = Command::new(HDIUTIL);
    command
        .args(["makehybrid", "-iso", "-joliet", "-udf", "-default-volume-name"])
        .arg(name)
        .arg("-o")
        .arg(dest)
        .arg(folder);
    command
}

pub fn parse_percent(line: &str) -> Option<f64> {
    let value: f64 = line.strip_prefix("PERCENT:")?.trim().parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value / 100.0)
}

pub fn read_lines(
    source: impl Read + Send + 'static,
    mut each: impl FnMut(&str) + Send + 'static
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(source).lines().map_while(io::Result::ok) {
            each(&line);
            all.push_str(&line);
            all.push('\n');
        }
        all
    })
}

pub fn folder_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).context(format!("Couldn't read {}", dir.display()))? {
            let entry = entry.context(format!("Couldn't read {}", dir.display()))?;
            let kind = entry.file_type().context("Couldn't inspect folder")?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(
                    entry
                        .metadata()
                        .map(|m| m.len())
                        .unwrap_or(0)
                );
            }
        }
    }
    Ok(total)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Raw,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Target {
    pub fn from_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("gz" | "gzip" | "tgz") => Self::Gzip,
            Some("bz2" | "bzip2") => Self::Bzip2,
            Some("xz") => Self::Xz,
            Some("zst" | "zstd") => Self::Zstd,
            _ => Self::Raw,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => "raw image",
            Self::Gzip => "gzip-compressed image",
            Self::Bzip2 => "bzip2-compressed image",
            Self::Xz => "xz-compressed image",
            Self::Zstd => "zstd-compressed image",
        }
    }

    pub fn encoder<W: Write>(self, sink: W) -> Result<Encoder<W>> {
        let threads = thread
            ::available_parallelism()
            .map_or(1, |n| n.get())
            .min(16) as u32;
        Ok(match self {
            Self::Raw => Encoder::Raw(sink),
            Self::Gzip =>
                Encoder::Gzip(flate2::write::GzEncoder::new(sink, flate2::Compression::default())),
            Self::Bzip2 =>
                Encoder::Bzip2(bzip2::write::BzEncoder::new(sink, bzip2::Compression::new(6))),
            Self::Xz => {
                let stream = liblzma::stream::MtStreamBuilder
                    ::new()
                    .threads(threads)
                    .preset(6)
                    .encoder()
                    .context("Couldn't start xz encoder")?;
                Encoder::Xz(liblzma::write::XzEncoder::new_stream(sink, stream))
            }
            Self::Zstd => {
                let mut encoder = zstd::stream::write::Encoder
                    ::new(sink, 3)
                    .context("Couldn't start zstd encoder")?;
                encoder.multithread(threads).context("Couldn't start zstd encoder")?;
                Encoder::Zstd(encoder)
            }
        })
    }
}

pub enum Encoder<W: Write> {
    Raw(W),
    Gzip(flate2::write::GzEncoder<W>),
    Bzip2(bzip2::write::BzEncoder<W>),
    Xz(liblzma::write::XzEncoder<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Encoder<W> {
    pub fn finish(self) -> io::Result<W> {
        match self {
            Self::Raw(mut w) => w.flush().map(|()| w),
            Self::Gzip(e) => e.finish(),
            Self::Bzip2(e) => e.finish(),
            Self::Xz(e) => e.finish(),
            Self::Zstd(e) => e.finish(),
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Raw(w) => w.write(buf),
            Self::Gzip(e) => e.write(buf),
            Self::Bzip2(e) => e.write(buf),
            Self::Xz(e) => e.write(buf),
            Self::Zstd(e) => e.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Raw(w) => w.write_all(buf),
            Self::Gzip(e) => e.write_all(buf),
            Self::Bzip2(e) => e.write_all(buf),
            Self::Xz(e) => e.write_all(buf),
            Self::Zstd(e) => e.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Raw(w) => w.flush(),
            Self::Gzip(e) => e.flush(),
            Self::Bzip2(e) => e.flush(),
            Self::Xz(e) => e.flush(),
            Self::Zstd(e) => e.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(target: Target, source: Source) {
        let data: Vec<u8> = (0..300_000u32).flat_map(|i| (i % 97).to_le_bytes()).collect();
        let mut encoder = target.encoder(Vec::new()).unwrap();
        encoder.write_all(&data).unwrap();
        let packed = encoder.finish().unwrap();
        let mut out = Vec::new();
        decoder(source, packed.as_slice()).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn compression_roundtrips() {
        roundtrip(Target::Gzip, Source::Gzip);
        roundtrip(Target::Bzip2, Source::Bzip2);
        roundtrip(Target::Xz, Source::Xz);
        roundtrip(Target::Zstd, Source::Zstd);
        roundtrip(Target::Raw, Source::Raw);
    }

    #[test]
    fn detects_by_content() {
        let dir = std::env::temp_dir().join(format!("ddgui-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cases: [(&str, Target, Source); 5] = [
            ("a.img", Target::Raw, Source::Raw),
            ("b.bin", Target::Gzip, Source::Gzip),
            ("c", Target::Bzip2, Source::Bzip2),
            ("d.img", Target::Xz, Source::Xz),
            ("e.raw", Target::Zstd, Source::Zstd),
        ];
        for (name, target, expected) in cases {
            let path = dir.join(name);
            let mut encoder = target.encoder(File::create(&path).unwrap()).unwrap();
            encoder.write_all(&[7u8; 4096]).unwrap();
            encoder.finish().unwrap();
            assert_eq!(Source::detect(&path).unwrap(), expected, "{name}");
        }
        assert_eq!(Source::detect(&dir).unwrap(), Source::Folder);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picks_targets_by_extension() {
        assert_eq!(Target::from_path(Path::new("x.img.GZ")), Target::Gzip);
        assert_eq!(Target::from_path(Path::new("x.zst")), Target::Zstd);
        assert_eq!(Target::from_path(Path::new("x.iso")), Target::Raw);
    }

    #[test]
    fn parses_unzip_listing() {
        let listing =
            "Archive:  x.zip\n  Length      Date    Time    Name\n---------  ---------- -----   ----\n        0  11-16-2026 14:29   dir/\n 3000000  11-16-2026 14:29   dir/my image.img\n      120  11-16-2026 14:29   __MACOSX/dir/._my image.img\n        6  11-16-2026 14:29   dir/.DS_Store\n---------                     -------\n  3000126                     4 files\n";
        let entries = parse_listing(listing);
        assert_eq!(entries, vec![ZipEntry { name: "dir/my image.img".into(), size: 3_000_000 }]);
        assert_eq!(pick_entry(entries).unwrap().name, "dir/my image.img");
    }

    #[test]
    fn picks_single_image_among_files() {
        let entries = vec![
            ZipEntry { name: "README.txt".into(), size: 10 },
            ZipEntry { name: "os.IMG".into(), size: 100 }
        ];
        assert_eq!(pick_entry(entries).unwrap().name, "os.IMG");
        let entries = vec![
            ZipEntry { name: "a.img".into(), size: 1 },
            ZipEntry { name: "b.iso".into(), size: 1 }
        ];
        assert!(pick_entry(entries).is_err());
    }

    #[test]
    fn escapes_zip_wildcards() {
        assert_eq!(zip_pattern("a[1]*?.img"), "a[[]1][*][?].img");
    }

    #[test]
    fn parses_puppet_percent() {
        assert_eq!(parse_percent("PERCENT:50.000000"), Some(0.5));
        assert_eq!(parse_percent("PERCENT:-1.000000"), None);
        assert_eq!(parse_percent("MESSAGE:x"), None);
    }
}
