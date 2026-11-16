use std::io::{ self, ErrorKind, Read, Write };
use std::sync::atomic::{ AtomicBool, Ordering };
use std::sync::mpsc;
use std::thread;

pub const CHUNK: usize = 4 << 20;
const DEPTH: usize = 3;

#[derive(Debug)]
pub enum PumpError {
    Read(io::Error),
    Write(io::Error),
}

pub fn pump<R, W>(
    source: R,
    sink: &mut W,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64)
)
    -> Result<u64, PumpError>
    where R: Read + Send, W: Write + ?Sized
{
    let stop = AtomicBool::new(false);
    let halted = || -> bool { stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) };
    let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(DEPTH);
    let (full_tx, full_rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(DEPTH);
    for _ in 0..DEPTH {
        let _ = free_tx.send(vec![0; CHUNK]);
    }
    thread::scope(|scope| {
        scope.spawn(move || {
            let mut source = source;
            while let Ok(mut buf) = free_rx.recv() {
                if halted() {
                    break;
                }
                buf.resize(CHUNK, 0);
                match fill(&mut source, &mut buf, &halted) {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        if full_tx.send(Ok(buf)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = full_tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        let mut total = 0u64;
        let result = loop {
            if cancel.load(Ordering::Relaxed) {
                break Ok(total);
            }
            match full_rx.recv() {
                Err(_) => {
                    break Ok(total);
                }
                Ok(Err(e)) => {
                    break Err(PumpError::Read(e));
                }
                Ok(Ok(buf)) => {
                    if let Err(e) = sink.write_all(&buf) {
                        break Err(PumpError::Write(e));
                    }
                    let n = buf.len() as u64;
                    total += n;
                    progress(n);
                    let _ = free_tx.send(buf);
                }
            }
        };
        stop.store(true, Ordering::Relaxed);
        drop(free_tx);
        drop(full_rx);
        result
    })
}

fn fill(source: &mut impl Read, buf: &mut [u8], halted: &impl Fn() -> bool) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() && !halted() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => {
                break;
            }
            Ok(n) => {
                filled += n;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(e);
            }
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_everything_in_order() {
        let data: Vec<u8> = (0..CHUNK * 2 + 12345).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        let mut seen = 0;
        let cancel = AtomicBool::new(false);
        let n = pump(data.as_slice(), &mut out, &cancel, |d| {
            seen += d;
        }).unwrap();
        assert_eq!(n as usize, data.len());
        assert_eq!(seen as usize, data.len());
        assert_eq!(out, data);
    }

    #[test]
    fn stops_when_cancelled() {
        let data = vec![1u8; CHUNK * 4];
        let mut out = Vec::new();
        let cancel = AtomicBool::new(true);
        let n = pump(data.as_slice(), &mut out, &cancel, |_| {}).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn reports_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let data = vec![0u8; 10];
        let cancel = AtomicBool::new(false);
        let err = pump(data.as_slice(), &mut Broken, &cancel, |_| {}).unwrap_err();
        assert!(matches!(err, PumpError::Write(e) if e.kind() == ErrorKind::BrokenPipe));
    }
}
