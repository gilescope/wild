//! Watches the compiled plugin dylib and reloads it whenever the
//! file mtime advances. No external crates beyond libloading — the
//! point is to show the mechanism, not ship a production reloader.
//!
//! Flow each tick:
//!   1. stat the plugin dylib.
//!   2. if mtime changed since last load, drop the old Library,
//!      copy the dylib to a side-path (so the next `cargo build`
//!      isn't blocked by our open handle on macOS / inode-locked
//!      mmap on linux), and `Library::new` it.
//!   3. call the plugin's `message()` and print it.
//!
//! Run pattern:
//!   term 1:  cargo run -p host
//!   term 2:  cargo build -p plugin   # then edit lib.rs, rerun

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

type MessageFn = unsafe extern "C" fn() -> *const u8;

fn plugin_src() -> PathBuf {
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("target/debug");
    if cfg!(target_os = "macos") {
        target.join("libplugin.dylib")
    } else if cfg!(target_os = "windows") {
        target.join("plugin.dll")
    } else {
        target.join("libplugin.so")
    }
}

fn load(version: u32) -> std::io::Result<(libloading::Library, SystemTime)> {
    let src = plugin_src();
    let mtime = std::fs::metadata(&src)?.modified()?;
    // Side-copy so the host's mmap doesn't race the next cargo link.
    let side = src.with_file_name(format!("plugin-v{version}.dylib.reload"));
    std::fs::copy(&src, &side)?;
    let lib = unsafe { libloading::Library::new(&side) }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok((lib, mtime))
}

fn main() -> std::io::Result<()> {
    let mut version = 0u32;
    let (mut lib, mut last_mtime) = load(version)?;
    println!("[host] loaded plugin v{version}");

    loop {
        {
            let sym: libloading::Symbol<MessageFn> =
                unsafe { lib.get(b"message\0").expect("message symbol") };
            let cstr = unsafe { CStr::from_ptr(sym() as *const c_char) };
            println!("[host] plugin says: {}", cstr.to_string_lossy());
        }

        std::thread::sleep(Duration::from_millis(500));

        if let Ok(mtime) = std::fs::metadata(plugin_src()).and_then(|m| m.modified())
            && mtime > last_mtime
        {
            drop(lib);
            version += 1;
            let (new_lib, new_mtime) = load(version)?;
            lib = new_lib;
            last_mtime = new_mtime;
            println!("[host] RELOADED → v{version}");
        }
    }
}
