//! The reloadable half. Edit `message`, rebuild (`cargo build -p plugin`),
//! and the host picks up the new string on the next tick.

#[unsafe(no_mangle)]
pub extern "C" fn message() -> *const u8 {
    b"hello from plugin v1\0".as_ptr()
}
