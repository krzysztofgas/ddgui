use std::collections::HashMap;
use std::process::Command;
use std::sync::{ Arc, Mutex };
use std::thread;
use std::time::Duration;

use plist::{ Dictionary, Value };

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

pub type Snapshot = Result<Vec<Disk>, String>;

#[derive(Default)]
pub struct Watch {
    pub latest: Option<Snapshot>,
}

pub fn spawn_watcher(shared: Arc<Mutex<Watch>>) {
    thread::spawn(move || {
        let mut cache: HashMap<String, Option<Disk>> = HashMap::new();
        let mut last: Option<Snapshot> = None;
        loop {
            let snapshot = scan(&mut cache);
            if last.as_ref() != Some(&snapshot) {
                if let Ok(mut guard) = shared.lock() {
                    guard.latest = Some(snapshot.clone());
                }
                last = Some(snapshot);
            }
            thread::sleep(Duration::from_millis(1500));
        }
    });
}

fn scan(cache: &mut HashMap<String, Option<Disk>>) -> Snapshot {
    let list = diskutil(&["list", "-plist"])?;
    let ids: Vec<String> = list
        .get("WholeDisks")
        .and_then(Value::as_array)
        .ok_or_else(|| "Unexpected output from diskutil list".to_string())?
        .iter()
        .filter_map(|v| v.as_string().map(str::to_owned))
        .collect();
    cache.retain(|id, _| ids.contains(id));
    let mut disks = Vec::new();
    for id in ids {
        if !cache.contains_key(&id) {
            match describe(&id) {
                Ok(disk) => {
                    cache.insert(id.clone(), disk);
                }
                Err(_) => {
                    continue;
                }
            }
        }
        if let Some(Some(disk)) = cache.get(&id) {
            disks.push(disk.clone());
        }
    }
    disks.sort_by_key(|d| natural_key(&d.id));
    Ok(disks)
}

fn natural_key(id: &str) -> (u64, String) {
    let digits: String = id.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u64::MAX), id.to_string())
}

fn describe(id: &str) -> Result<Option<Disk>, String> {
    let info = diskutil(&["info", "-plist", id])?;
    let flag = |key: &str| info.get(key).and_then(Value::as_boolean);
    let text = |key: &str| {
        info.get(key)
            .and_then(Value::as_string)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if flag("Internal").unwrap_or(true) {
        return Ok(None);
    }
    if text("VirtualOrPhysical").as_deref() == Some("Virtual") {
        return Ok(None);
    }
    if text("BusProtocol").as_deref() == Some("Disk Image") {
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
        .unwrap_or_else(|| "Untitled".to_string());
    Ok(
        Some(Disk {
            id: id.to_string(),
            name,
            size,
        })
    )
}

fn diskutil(args: &[&str]) -> Result<Dictionary, String> {
    let output = Command::new("/usr/sbin/diskutil")
        .args(args)
        .output()
        .map_err(|e| format!("Couldn't run diskutil: {e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(
            if msg.is_empty() {
                format!("diskutil failed ({})", output.status)
            } else {
                msg
            }
        );
    }
    Value::from_reader_xml(output.stdout.as_slice())
        .map_err(|e| format!("Couldn't parse diskutil output: {e}"))?
        .into_dictionary()
        .ok_or_else(|| "Unexpected output from diskutil".to_string())
}
