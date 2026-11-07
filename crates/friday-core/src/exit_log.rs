//! 退出记录（<缓存目录>/exit.log）：桌面端什么时候起来、谁让它退的、有没有走完退出。窗口程序没有别的日志，
//! 用户说「莫名其妙退出了」时靠它查（2026-11-07 有一次退出就因为没有这份记录，查不出是谁发起的）。
//!
//! - 启动时放标记 exit.running（进程号、版本、启动时间），走完退出删掉。下次启动时标记还在，就记一行「上次没有正常结束」：
//!   崩溃、被强行结束、断电、退出时卡住都这样现形
//! - 每行当场写下，不走后台写盘：进程可能马上就没了
//! - 文件超过 256 KB 时换成 exit.log.1，只留这一份旧的

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const LIMIT: u64 = 256 * 1024;

struct Log {
    file: PathBuf,
    marker: PathBuf,
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

impl Log {
    fn append(&self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        let line = format!("{} [{}] {text}\n", now(), std::process::id());
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.file) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// 开始记。takeover：这次是接管或重启，上一个进程这时可能还没退完，它的标记不算没正常结束
pub fn start(directory: &Path, version: &str, takeover: bool) {
    let _ = fs::create_dir_all(directory);
    let log = Log { file: directory.join("exit.log"), marker: directory.join("exit.running") };
    if fs::metadata(&log.file).is_ok_and(|metadata| metadata.len() > LIMIT) {
        let _ = fs::rename(&log.file, directory.join("exit.log.1"));
    }
    if !takeover && let Ok(previous) = fs::read_to_string(&log.marker) {
        log.append(&format!("上次运行没有正常结束（{}）：崩溃、被强行结束、断电，或退出时卡住", previous.trim()));
    }
    let _ = fs::write(&log.marker, format!("进程 {}，版本 {version}，启动于 {}", std::process::id(), now()));
    log.append(&format!("启动 {version}"));
    *LOG.lock().unwrap() = Some(log);
}

/// 记一行（还没 start 时什么也不做）
pub fn note(text: &str) {
    if let Some(log) = LOG.lock().unwrap().as_ref() {
        log.append(text);
    }
}

/// 走完退出：记一行、收掉标记。标记已经是接管的新进程的了就留着
pub fn finish() {
    if let Some(log) = LOG.lock().unwrap().take() {
        log.append("退出完成");
        let mine = format!("进程 {}，", std::process::id());
        if fs::read_to_string(&log.marker).is_ok_and(|marker| marker.starts_with(&mine)) {
            let _ = fs::remove_file(&log.marker);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::api::call;

    #[test]
    fn records_start_notes_and_unclean_exit() {
        let directory = std::env::temp_dir().join(format!("friday-core-exit-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let start = |takeover: bool| {
            let params = json!({ "directory": directory, "version": "v1", "takeover": takeover });
            assert_eq!(call("exitLog.start", &params.to_string()), r#"{"result":null}"#);
        };
        let note = |text: &str| call("exitLog.note", &json!({ "text": text }).to_string());
        let log = || std::fs::read_to_string(directory.join("exit.log")).unwrap_or_default();
        let marker = directory.join("exit.running");

        // 还没 start：不记
        note("早了");
        assert!(log().is_empty());
        // 走完退出：标记收掉，下次不报
        start(false);
        assert!(marker.exists());
        note("用户点了\n退出");
        call("exitLog.finish", "");
        assert!(!marker.exists());
        assert!(log().contains("启动 v1"));
        assert!(log().contains("用户点了 退出"));
        assert!(log().contains("退出完成"));
        start(false);
        assert!(!log().contains("没有正常结束"));
        // 这一次没走完（相当于崩了）：下次启动记一行，新的标记接着放
        start(false);
        assert!(log().contains("上次运行没有正常结束（进程"));
        assert!(marker.exists());
        // 接管时上一个可能还没退完：不算没正常结束；标记是别的进程的不删
        let before = log();
        start(true);
        assert_eq!(log().matches("没有正常结束").count(), before.matches("没有正常结束").count());
        std::fs::write(&marker, "进程 0，版本 v2，启动于 刚才").unwrap();
        call("exitLog.finish", "");
        assert!(marker.exists());
        // 太大了换成 exit.log.1
        std::fs::write(directory.join("exit.log"), "x".repeat(super::LIMIT as usize + 1)).unwrap();
        start(true);
        assert!(directory.join("exit.log.1").exists());
        assert!(log().len() < 1024);
        call("exitLog.finish", "");
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
