use std::collections::{ BTreeSet, HashMap };
use std::ffi::{ CStr, CString };
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{ Child, Command, Stdio };
use std::sync::{ Arc, Mutex };
use std::thread;
use std::time::{ Duration, Instant };

use plist::{ Dictionary, Value };

use crate::error::{ Context, Error, Result, bail, lock };

const DISKUTIL: &str = "/usr/sbin/diskutil";
const POLL: Duration = Duration::from_millis(1500);
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub id: String,
    pub name: String,
    pub size: u64,
}

impl Disk {
    pub fn node(&self) -> String {
        format!("/dev/{}", self.id)
    }

    pub fn raw_node(&self) -> String {
        format!("/dev/r{}", self.id)
    }
}

pub type Snapshot = Result<Vec<Disk>>;

#[derive(Default)]
pub struct Watch {
    pub latest: Option<Snapshot>,
}

pub fn spawn_watcher(shared: Arc<Mutex<Watch>>) -> Result<()> {
    thread::Builder
        ::new()
        .name("disk-watch".into())
        .spawn(move || {
            let mut cache = HashMap::new();
            let mut protected = None;
            let mut last = None;
            loop {
                if protected.is_none() {
                    protected = protected_disks().ok();
                }
                let snapshot = scan(&mut cache, protected.as_ref());
                if last.as_ref() != Some(&snapshot) {
                    lock(&shared).latest = Some(snapshot.clone());
                    last = Some(snapshot);
                }
                thread::sleep(POLL);
            }
        })
        .map(drop)
        .context("Couldn't start the device monitor")
}

fn scan(
    cache: &mut HashMap<String, Option<Disk>>,
    protected: Option<&BTreeSet<String>>
) -> Snapshot {
    let list = diskutil(&["list", "-plist"])?;
    let ids: Vec<String> = list
        .get("WholeDisks")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("Unexpected output from diskutil list"))?
        .iter()
        .filter_map(Value::as_string)
        .map(str::to_owned)
        .collect();
    cache.retain(|id, _| ids.contains(id));
    let mut disks: Vec<Disk> = ids
        .iter()
        .filter(|id| protected.is_none_or(|p| !p.contains(*id)))
        .filter_map(|id| {
            if !cache.contains_key(id) {
                cache.insert(id.clone(), describe(id).ok()?);
            }
            cache.get(id).cloned().flatten()
        })
        .collect();
    disks.sort_by_key(|d| natural_key(&d.id));
    Ok(disks)
}

fn natural_key(id: &str) -> (u64, String) {
    let digits: String = id.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u64::MAX), id.to_owned())
}

pub fn lookup(id: &str) -> Result<Disk> {
    if protected_disks()?.contains(id) {
        bail!("{id} holds the startup disk and can't be used");
    }
    describe(id)?.ok_or_else(|| Error::new(format!("{id} is not an external device")))
}

fn describe(id: &str) -> Result<Option<Disk>> {
    let info = diskutil(&["info", "-plist", id])?;
    let flag = |key: &str| info.get(key).and_then(Value::as_boolean);
    let text = |key: &str| {
        info.get(key)
            .and_then(Value::as_string)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let excluded =
        flag("Internal").unwrap_or(true) ||
        flag("WholeDisk") == Some(false) ||
        text("VirtualOrPhysical").as_deref() == Some("Virtual") ||
        text("BusProtocol").as_deref() == Some("Disk Image");
    if excluded {
        return Ok(None);
    }
    let size = ["TotalSize", "Size"]
        .iter()
        .find_map(|k| info.get(k).and_then(Value::as_unsigned_integer))
        .unwrap_or(0);
    if size == 0 {
        return Ok(None);
    }
    let name = text("MediaName")
        .or_else(|| text("IORegistryEntryName"))
        .unwrap_or_else(|| "Untitled".to_owned());
    Ok(
        Some(Disk {
            id: id.to_owned(),
            name,
            size,
        })
    )
}

pub fn protected_disks() -> Result<BTreeSet<String>> {
    disks_for_path(Path::new("/"))
}

pub fn disks_for_path(path: &Path) -> Result<BTreeSet<String>> {
    let Some(whole) = mount_source(path)?.as_deref().and_then(whole_disk) else {
        return Ok(BTreeSet::new());
    };
    let mut found = BTreeSet::new();
    let mut pending = vec![whole];
    while let Some(id) = pending.pop() {
        if !found.insert(id.clone()) {
            continue;
        }
        let info = diskutil(&["info", "-plist", &id])?;
        let stores = info
            .get("APFSPhysicalStores")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_dictionary)
            .filter_map(|d| d.get("APFSPhysicalStore").and_then(Value::as_string))
            .filter_map(whole_disk);
        pending.extend(stores);
    }
    Ok(found)
}

fn mount_source(path: &Path) -> Result<Option<String>> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_|
        Error::new(format!("Invalid path {}", path.display()))
    )?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if (unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) }) != 0 {
        return Err(std::io::Error::last_os_error()).context(
            format!("Couldn't inspect {}", path.display())
        );
    }
    let stat = unsafe { stat.assume_init() };
    let source = unsafe { CStr::from_ptr(stat.f_mntfromname.as_ptr()) };
    Ok(source.to_str().ok().map(str::to_owned))
}

pub fn whole_disk(node: &str) -> Option<String> {
    let name = node.strip_prefix("/dev/").unwrap_or(node);
    let name = name
        .strip_prefix('r')
        .filter(|n| n.starts_with("disk"))
        .unwrap_or(name);
    let digits = name.strip_prefix("disk")?;
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    (end > 0).then(|| format!("disk{}", &digits[..end]))
}

fn diskutil(args: &[&str]) -> Result<Dictionary> {
    let mut child = Command::new(DISKUTIL)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Couldn't run diskutil")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (out, err, status) = thread::scope(|s| {
        let out = s.spawn(move || read_all(stdout));
        let err = s.spawn(move || read_all(stderr));
        let status = wait_timeout(&mut child, TIMEOUT);
        (out.join().unwrap_or_default(), err.join().unwrap_or_default(), status)
    });
    let status = status?;
    if !status.success() {
        let msg = String::from_utf8_lossy(&err).trim().to_owned();
        if msg.is_empty() {
            bail!("diskutil failed ({status})");
        }
        return Err(Error::new(msg));
    }
    Value::from_reader_xml(out.as_slice())
        .context("Couldn't parse diskutil output")?
        .into_dictionary()
        .ok_or_else(|| Error::new("Unexpected output from diskutil"))
}

fn read_all(source: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut source) = source {
        let _ = source.read_to_end(&mut buf);
    }
    buf
}

fn wait_timeout(child: &mut Child, limit: Duration) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().context("Couldn't wait for diskutil")? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("diskutil did not respond");
        }
        thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_disk_names() {
        assert_eq!(whole_disk("/dev/disk3s3s1").as_deref(), Some("disk3"));
        assert_eq!(whole_disk("/dev/rdisk12").as_deref(), Some("disk12"));
        assert_eq!(whole_disk("disk0s2").as_deref(), Some("disk0"));
        assert_eq!(whole_disk("map auto_home"), None);
        assert_eq!(whole_disk("/dev/disk"), None);
    }

    #[test]
    fn natural_order() {
        let mut ids = vec!["disk10", "disk2", "disk4"];
        ids.sort_by_key(|id| natural_key(id));
        assert_eq!(ids, ["disk2", "disk4", "disk10"]);
    }
}
