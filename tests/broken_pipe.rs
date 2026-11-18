//! A reader that closes the pipe early (`packctl list | head -1`) must not
//! make packctl panic.

use std::process::{Command, Stdio};

#[test]
fn closed_stdout_is_not_a_panic() {
    let home = tempfile::tempdir().unwrap();
    // The read end is closed before packctl starts, so its first write to
    // stdout fails with a broken pipe.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);

    let output = Command::new(env!("CARGO_BIN_EXE_packctl"))
        .arg("list")
        .env("PACKCTL_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(output.status.success(), "{:?}: {stderr}", output.status);
}
