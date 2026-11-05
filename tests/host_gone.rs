use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn exits_cleanly_when_host_stdout_and_stderr_are_gone() {
    let mut plugin = Command::new(env!("CARGO_BIN_EXE_planeai-plugin-github"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(plugin.stdout.take());
    drop(plugin.stderr.take());
    let mut stdin = plugin.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"plugin.handshake","params":{{}}}}"#
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = plugin.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "plugin kept running");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(0));
}
