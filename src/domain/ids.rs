use std::sync::atomic::{AtomicU64, Ordering};

static INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn new_instance_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{sequence:x}", std::process::id())
}
