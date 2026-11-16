# ddgui

A minimalist, native macOS front end for `dd`. Write disk images to USB drives and SD cards, or back a device up to an image file, with live progress and automatic light/dark appearance.

## Features

- **Write Image**: flash an image or folder to an external device.
- **Read Device**: save a device to an image file, optionally compressed.
- **Live updates**: devices appear and disappear as they are plugged in; progress shows bytes, speed and time left.
- **Many formats**, detected by content rather than extension:

  | Input | Handling |
  | --- | --- |
  | `.img` `.iso` `.bin` `.raw` | written as-is |
  | `.gz` `.bz2` `.xz` `.zst` | decompressed on the fly |
  | `.zip` | the image inside is extracted on the fly |
  | `.dmg` | converted to a raw image first |
  | folder | packed into an ISO 9660 / Joliet / UDF image first |

  When reading a device, end the file name with `.gz`, `.bz2`, `.xz` or `.zst` to compress the backup.

- **Extra dd operands**: the optional *Options* field accepts operands such as `bs=1m conv=sync status=none`. `if=` and `of=` are set by the app.

## Safety

- Only external, physical whole disks are listed. The startup disk and its APFS backing stores are always excluded.
- The device is re-checked right before starting, so a swapped drive with the same identifier is caught.
- Writing an image that lives on the target device, or saving a backup onto the device being read, is refused.
- Image size and free space are checked up front where possible.
- Erasing needs an explicit click; Return cancels.
- Backups are written to a hidden temporary file and renamed only on success. Cancelled or failed backups leave nothing behind.
- The Mac is kept awake during a copy, and closing or quitting mid-copy asks first.

## Requirements

- macOS 10.12 or later on Intel, macOS 11 or later on Apple Silicon.
- An administrator account. dd runs as root through the standard macOS password prompt.

## Building

Requires a Rust toolchain (via [rustup](https://rustup.rs)) and the Xcode command-line tools.

```sh
./bundle.sh
```

This builds a universal binary and creates `target/ddgui.app`. For development:

```sh
cargo run
cargo test
```

## How it works

ddgui unmounts the device and runs `dd` against the raw device node (`/dev/rdiskN`) with 4 MB blocks, through `osascript … with administrator privileges`. Data passes between the app and dd through a private named pipe. This lets the app decompress or compress the stream, count every byte for exact progress, and stop dd cleanly on cancel. Device discovery uses `diskutil`; `.dmg` conversion and folder packing use `hdiutil`.

## Limitations

- The final size of a compressed image is not known in advance. One that is larger than the device fails when dd reaches the end of the device.
- Encrypted `.dmg` files are not supported.
- Custom options that end the copy early (such as `count=`) are judged by dd's exit status rather than by the full size.
