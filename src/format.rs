use std::time::Duration;

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} bytes");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 99.95 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(1000), "1.0 KB");
        assert_eq!(bytes(31_914_983_424), "31.9 GB");
        assert_eq!(bytes(99_960_000), "100 MB");
        assert_eq!(bytes(u64::MAX), "18447 PB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(Duration::from_secs(5)), "0:05");
        assert_eq!(duration(Duration::from_secs(605)), "10:05");
        assert_eq!(duration(Duration::from_secs(3725)), "1:02:05");
    }
}
