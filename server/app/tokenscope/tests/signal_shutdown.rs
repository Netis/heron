#[cfg(unix)]
mod unix_only {
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    #[test]
    fn sigterm_exits_promptly() {
        let tmp = tempdir().expect("create tempdir");
        let db_path = tmp.path().join("signal-test.duckdb");
        let config_path = tmp.path().join("signal-test.toml");

        let config = format!(
            r#"[storage]
backend = "duckdb"

[storage.duckdb]
path = "{}"

[storage.sink]
batch_size = 16
flush_interval_ms = 10

[storage.retention]
enabled = false
check_interval_secs = 1
calls = 0
turns = 0
http_exchanges = 0

[internal_metrics]
enabled = true
interval_secs = 1

[api]
listen = "127.0.0.1"
port = 0
"#,
            db_path.display()
        );
        fs::write(&config_path, config).expect("write config");

        let bin = env!("CARGO_BIN_EXE_tokenscope");
        let mut child = Command::new(bin)
            .arg("--config")
            .arg(&config_path)
            .spawn()
            .expect("spawn tokenscope");

        thread::sleep(Duration::from_millis(500));

        let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0, "send SIGTERM");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("poll child exit") {
                assert!(
                    status.success() || status.signal() == Some(libc::SIGTERM),
                    "tokenscope should exit promptly on SIGTERM, got {status}"
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("tokenscope did not exit within 5s after SIGTERM");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}
