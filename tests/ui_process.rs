//! Run explicitly on an interactive Windows desktop:
//! cargo test --test ui_process -- --ignored --test-threads=1
#![cfg(target_os = "windows")]

use std::{
    io::{BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

struct UiFixture {
    child: Child,
    input: Option<ChildStdin>,
}

impl UiFixture {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kemolight"))
            .arg("--ui-child")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        // Drain output concurrently: both anonymous pipes have finite buffers.
        let output = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if line.is_err() {
                    break;
                }
            }
        });
        let mut fixture = Self { child, input };
        fixture.send(json!({"Initialize": {
            "config": {"start_hidden": false, "brightness_step": 5, "preferred_backend": "mock"},
            "snapshot": {"displays": [], "last_error": null, "profiles": {"bright": {}, "dark": {}}, "profile_message": null, "profile_error": null, "hotkey_error": null, "lid_closed": false},
            "geometry": {"inner_size": [560, 320], "outer_position": null}, "tray_available": true
        }}));
        fixture
    }

    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &value).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }

    fn assert_exits(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "UI exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "UI process did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for UiFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop with OpenGL"]
fn hide_exits_ui_process_and_a_new_session_can_start() {
    for _ in 0..3 {
        let mut ui = UiFixture::start();
        thread::sleep(Duration::from_millis(300));
        ui.send(json!("Hide"));
        ui.assert_exits();
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop with OpenGL"]
fn losing_parent_pipe_exits_ui_process() {
    let mut ui = UiFixture::start();
    drop(ui.input.take());
    ui.assert_exits();
}
