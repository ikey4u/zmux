#[cfg(unix)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use zmux_core::{managed::ManagedRuntime, Environment};
    fn runtime() -> (PathBuf, ManagedRuntime) {
        static NEXT: std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "managed-{}-{}",
            NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let rt = ManagedRuntime::new(
            Environment::at(root.join("zmux"), "test").unwrap(),
        )
        .unwrap();
        (root, rt)
    }
    fn wait(rt: &ManagedRuntime, id: &str, p: usize, expected: &str) {
        let mut after = None;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        loop {
            let r = rt.pull(id, p, after, Duration::from_millis(100)).unwrap();
            after = Some(r.offset);
            text.push_str(&String::from_utf8_lossy(&r.data));
            if text.contains(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "missing {expected}: {text}"
            );
        }
    }
    #[test]
    fn typed_management_uses_real_zmux_windows_and_panes() {
        let (root, rt) = runtime();
        let s = rt.open(&root, "Work", Some("/bin/sh"), 80, 24).unwrap();
        let p = s.windows[0].panes[0].id.parse().unwrap();
        let w = s.windows[0].id.parse().unwrap();
        rt.input(&s.id, p, b"printf 'FIRST_%s\\n' READY\r").unwrap();
        wait(&rt, &s.id, p, "FIRST_READY");
        let (w2, p2) = rt.create(&s.id, None, None, "Checks").unwrap();
        let (_, p3) = rt.create(&s.id, Some(w), Some(p), "Shell").unwrap();
        let list = rt.list().unwrap();
        assert_eq!(list[0].windows.len(), 2);
        assert_eq!(list[0].windows[0].panes.len(), 2);
        rt.resize(&s.id, p, 41, 19).unwrap();
        rt.resize(&s.id, p2.parse().unwrap(), 93, 31).unwrap();
        let list = rt.list().unwrap();
        assert_eq!(list[0].windows[0].panes[0].cols, 41);
        assert_eq!(list[0].windows[1].panes[0].cols, 93);
        rt.input(&s.id, p2.parse().unwrap(), b"printf 'SECOND_%s\\n' READY\r")
            .unwrap();
        wait(&rt, &s.id, p2.parse().unwrap(), "SECOND_READY");
        rt.close(&s.id, Some(w), Some(p3.parse().unwrap())).unwrap();
        assert!(rt.input(&s.id, p3.parse().unwrap(), b"bad").is_err());
        assert_eq!(rt.list().unwrap()[0].windows[0].panes.len(), 1);
        rt.close(&s.id, Some(w2.parse().unwrap()), None).unwrap();
        assert_eq!(rt.list().unwrap()[0].windows.len(), 1);
        rt.close(&s.id, None, None).unwrap();
        assert!(rt.list().unwrap().is_empty());
        drop(rt);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn viewer_detach_and_other_sessions_do_not_end_shells() {
        let (root, rt) = runtime();
        let a = rt.open(&root, "A", None, 80, 24).unwrap();
        let b = rt.open(&root, "B", None, 80, 24).unwrap();
        let p = b.windows[0].panes[0].id.parse().unwrap();
        rt.close(&a.id, None, None).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        rt.input(&b.id, p, b"printf 'STILL_%s\\n' ALIVE\r").unwrap();
        wait(&rt, &b.id, p, "STILL_ALIVE");
        rt.close(&b.id, None, None).unwrap();
        drop(rt);
        std::fs::remove_dir_all(root).unwrap();
    }
}
