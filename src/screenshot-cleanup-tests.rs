// Purpose: Verify screenshot cancellation terminates owned subprocesses and removes owned temp files.

use super::*;

#[tokio::test]
async fn cancelled_capture_read_removes_owned_path() {
    let path = temp_png_path("cancelled-read-fixture");
    fs::write(&path, b"partial").unwrap();
    let owned = path.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(finish_capture_read(
        ScreenshotCleanup::DeletePath(owned),
        async move {
            let _ = started.send(());
            std::future::pending::<Result<RawScreenshotCapture>>().await
        },
    ));
    ready.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!path.exists());
}

#[tokio::test]
async fn cancelled_screenshot_command_kills_process_and_cleans_path() {
    let path = temp_png_path("cancelled-command-fixture");
    let pid_path = temp_png_path("cancelled-command-pid");
    fs::write(&path, b"partial").unwrap();
    let owned = path.clone();
    let pid_file = pid_path.clone();
    let task = tokio::spawn(async move {
        let _cleanup = ScreenshotCleanup::DeletePath(owned.clone());
        let mut command = Command::new("sh");
        command.args(["-c", "echo $$ > \"$1\"; exec sleep 60", "fixture"]);
        command.arg(pid_file);
        wait_screenshot_command(&mut command).await
    });
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = fs::read_to_string(&pid_path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!path.exists());
    tokio::time::timeout(Duration::from_secs(2), async {
        while PathBuf::from(format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fs::remove_file(pid_path).unwrap();
}
