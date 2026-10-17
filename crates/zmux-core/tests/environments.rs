use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use zmux_core::{environment, Environment, TerminalRuntime};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "zmux-env-test-{}",
            zmux_core::domain::new_instance_id()
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn paths_locks_and_config_are_separate_even_for_identical_socket_names() {
    let f = Fixture::new();
    let a = Environment::at(&f.0, "default").unwrap();
    let b = Environment::at(&f.0, "sloud-worker").unwrap();
    a.prepare().unwrap();
    b.prepare().unwrap();
    fs::write(a.config_file(), "set -g history-limit 123\n").unwrap();
    assert_ne!(a.directory(), b.directory());
    assert_ne!(a.history_directory(), b.history_directory());
    assert_ne!(a.drop_directory(), b.drop_directory());
    assert_ne!(a.machines_file(), b.machines_file());
    assert_ne!(a.log_directory(), b.log_directory());
    assert!(b.enter(zmux_core::config::find_config_file).is_none());
    assert_eq!(
        b.enter(zmux_core::config::machines::config_path).unwrap(),
        b.machines_file()
    );
    #[cfg(unix)]
    assert_ne!(
        a.enter(|| zmux_core::ipc::socket_path("same")).unwrap(),
        b.enter(|| zmux_core::ipc::socket_path("same")).unwrap()
    );
    let lock = a.server_lock("same").unwrap();
    assert!(a.server_lock("same").is_err());
    let other = b.server_lock("same").unwrap();
    drop(lock);
    assert!(a.server_lock("same").is_ok());
    drop(other);
}
#[test]
fn explicit_scopes_restore_and_threads_inherit_without_changing_process_environment(
) {
    let f = Fixture::new();
    let a = Environment::at(&f.0, "one").unwrap();
    let b = Environment::at(&f.0, "two").unwrap();
    let before = std::env::var_os("ZMUX_ENVIRONMENT_ID");
    a.enter(|| {
        assert_eq!(environment::current().id(), "one");
        b.enter(|| assert_eq!(environment::current().id(), "two"));
        assert_eq!(environment::current().id(), "one");
        let t = environment::spawn(|| environment::current().id().to_owned());
        assert_eq!(t.join().unwrap(), "one");
    });
    assert_eq!(environment::current().id(), "default");
    assert_eq!(before, std::env::var_os("ZMUX_ENVIRONMENT_ID"));
    let _ = std::panic::catch_unwind(|| a.enter(|| panic!("scope unwind")));
    assert_eq!(environment::current().id(), "default");
}
#[test]
fn unsafe_names_and_root_aliases_cannot_escape_or_bypass_locks() {
    let f = Fixture::new();
    for id in [
        "",
        "../default",
        "Prod",
        "CON",
        "con",
        "a/b",
        "a\\b",
        "-bad",
        "中文",
    ] {
        assert!(Environment::at(&f.0, id).is_err(), "{id}")
    }
    assert!(Environment::at("relative", "default").is_err());
    assert!(Environment::at(f.0.join("../elsewhere"), "default").is_err());
    let env = Environment::at(&f.0, "test").unwrap();
    for s in ["../escape", "/tmp/escape", "a/b", "a\\b", ""] {
        assert!(env.server_lock(s).is_err())
    }
    #[cfg(unix)]
    {
        let alias = f.0.with_extension("alias");
        std::os::unix::fs::symlink(&f.0, &alias).unwrap();
        let other = Environment::at(&alias, "test").unwrap();
        assert_eq!(env.namespace_key(), other.namespace_key());
        let lock = env.server_lock("host").unwrap();
        assert!(other.server_lock("host").is_err());
        drop(lock);
        fs::remove_file(alias).unwrap();
    }
}
#[test]
fn protocol_rejects_cross_environment_before_commands() {
    let f = Fixture::new();
    let a = Environment::at(&f.0, "default").unwrap();
    let b = Environment::at(&f.0, "worker").unwrap();
    let info = a.enter(zmux_core::ipc::ProtocolInfo::current);
    let peer = b.enter(zmux_core::ipc::ProtocolInfo::current);
    assert_eq!(
        zmux_core::ipc::negotiate(&info, &peer).unwrap_err().code,
        "environment_mismatch"
    );
}
fn rendered(bytes: &[u8]) -> String {
    let mut term = zmux_core::terminal::AlacrittyTermState::new(24, 80, 100);
    term.process(bytes);
    term.snapshot_rows()
        .0
        .into_iter()
        .map(|row| {
            row.into_iter()
                .flatten()
                .map(|cell| cell.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn wait_for(runtime: &TerminalRuntime, id: &str, needle: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    let mut offset = 0;
    while Instant::now() < deadline {
        let out = runtime
            .pull(id, offset, Duration::from_millis(100))
            .unwrap();
        offset = out.offset;
        all.extend(out.data);
        if rendered(&all).contains(needle) {
            return all;
        }
    }
    panic!("missing {needle} in {}", rendered(&all))
}
#[test]
fn independent_headless_runtimes_keep_shells_and_frames_separate() {
    let f = Fixture::new();
    let a = Environment::at(&f.0, "default").unwrap();
    let b = Environment::at(&f.0, "worker").unwrap();
    let one = TerminalRuntime::new(a.clone()).unwrap();
    assert!(TerminalRuntime::new(a).is_err());
    let two = TerminalRuntime::new(b).unwrap();
    let first = one.open(&f.0, 80, 24).unwrap();
    let second = two.open(&f.0, 60, 18).unwrap();
    assert!(two.input(&first, b"ignored").is_err());
    assert!(two.resize(&first, 30, 10).is_err());
    assert!(!two.close(&first));
    // Literal strings are split so terminal echo cannot satisfy the output assertion.
    one.input(&first, b"printf 'ONLY_%s\\n' ONE\r").unwrap();
    two.input(&second, b"printf 'ONLY_%s\\n' TWO\r").unwrap();
    let data = wait_for(&one, &first, "ONLY_ONE");
    assert!(!rendered(&data).contains("ONLY_TWO"));
    wait_for(&two, &second, "ONLY_TWO");
    // No client or pull is needed for the shell to remain alive.
    std::thread::sleep(Duration::from_millis(80));
    one.input(&first, b"printf 'ENV_%s\\n' \"$ZMUX_ENVIRONMENT_ID\"\r")
        .unwrap();
    wait_for(&one, &first, "ENV_default");
    assert!(one.close(&first));
    drop(one);
    two.input(&second, b"printf 'STILL_%s\\n' ALIVE\r").unwrap();
    wait_for(&two, &second, "STILL_ALIVE");
    assert!(two.close(&second));
    let third = two.open(&f.0, 80, 24).unwrap();
    assert_ne!(second, third);
    assert!(two.close(&third));
}

#[cfg(unix)]
#[test]
fn headless_view_uses_the_requested_rows_without_desktop_chrome() {
    let f = Fixture::new();
    let env = Environment::at(&f.0, "sizing").unwrap();
    let rt = TerminalRuntime::new(env).unwrap();
    let id = rt.open(&f.0, 60, 18).unwrap();
    rt.input(&id, b"printf 'BEFORE_'; stty size\r").unwrap();
    wait_for(&rt, &id, "BEFORE_18 60");
    rt.resize(&id, 80, 24).unwrap();
    rt.input(&id, b"printf 'AFTER_'; stty size\r").unwrap();
    wait_for(&rt, &id, "AFTER_24 80");
    assert!(rt.close(&id));
}

#[test]
fn identical_ids_in_different_application_roots_have_distinct_hosts() {
    let f = Fixture::new();
    let a = Environment::at(f.0.join("one"), "sloud-worker").unwrap();
    let b = Environment::at(f.0.join("two"), "sloud-worker").unwrap();
    assert_ne!(a.namespace_key(), b.namespace_key());
    let one = TerminalRuntime::new(a).unwrap();
    let two = TerminalRuntime::new(b).unwrap();
    drop((one, two));
}
