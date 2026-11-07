//! 后台写盘（== Qt 的 OrderedSave）：调用方交一份快照就返回，后台线程按顺序写；同一个文件排着好几份时只写最新的。
//! 先写同目录的临时文件再换名，写到一半不会留下半截文件。退出前调 [`flush`]（C 接口 `core.flush`）等排着的都写完。
//!
//! 内容边生成边写进临时文件，不先在内存里拼好：存档有两百 MB，整份拼进 Vec 时缓冲翻倍要一次拿 256 MB，
//! 机器的提交内存见顶时分配失败，Rust 直接中止整个进程（2026-11-05 这样崩过）。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, LazyLock, Mutex};

/// 把内容写进给的文件；出错时这次不换名，原来的文件不动
type Render = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

/// 写临时文件的缓冲（serde_json 一次只写几个字节）
const BUFFER: usize = 1 << 20;

struct Writer {
    sender: Mutex<Sender<(PathBuf, Render)>>,
    /// 交了还没写完的份数
    pending: Mutex<usize>,
    idle: Condvar,
}

static WRITER: LazyLock<Writer> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new().name("friday-core-writer".to_string()).spawn(move || run(receiver)).expect("起不了写盘线程");
    Writer { sender: Mutex::new(sender), pending: Mutex::new(0), idle: Condvar::new() }
});

fn run(receiver: Receiver<(PathBuf, Render)>) {
    while let Ok(first) = receiver.recv() {
        // 一次把排着的都拿出来：同一个文件只写最后交的那份
        let mut batch = vec![first];
        batch.extend(receiver.try_iter());
        let total = batch.len();
        let mut latest: HashMap<PathBuf, usize> = HashMap::new();
        for (index, (path, _)) in batch.iter().enumerate() {
            latest.insert(path.clone(), index);
        }
        for (index, (path, render)) in batch.into_iter().enumerate() {
            if latest.get(&path) == Some(&index) {
                let _ = write_with(&path, render);
            }
        }
        let mut pending = WRITER.pending.lock().unwrap();
        *pending -= total;
        if *pending == 0 {
            WRITER.idle.notify_all();
        }
    }
}

/// 交一份要写的内容（render 在后台线程里调，把快照写进给的文件）
pub fn write(path: PathBuf, render: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static) {
    *WRITER.pending.lock().unwrap() += 1;
    if let Err(mpsc::SendError((path, render))) = WRITER.sender.lock().unwrap().send((path, Box::new(render))) {
        // 写盘线程没了（不该发生）：就地写
        let _ = write_with(&path, render);
        *WRITER.pending.lock().unwrap() -= 1;
    }
}

/// 等排着的都写完
pub fn flush() {
    let mut pending = WRITER.pending.lock().unwrap();
    while *pending > 0 {
        pending = WRITER.idle.wait(pending).unwrap();
    }
}

/// 先写同目录的临时文件再换名
pub fn write_now(path: &Path, data: &[u8]) -> io::Result<()> {
    write_with(path, |file| file.write_all(data))
}

/// render 把内容写进同目录的临时文件，写完再换名；render 出错时删掉临时文件，原来的文件不动
fn write_with(path: &Path, render: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let temporary = path.with_file_name(format!(".{name}.friday-core-{}", std::process::id()));
    let written = File::create(&temporary).and_then(|file| {
        let mut file = BufWriter::with_capacity(BUFFER, file);
        render(&mut file)?;
        file.flush()
    });
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    // Windows 上目标正被别的程序开着（后台的 git add 在读、杀毒软件在扫）时换名会被拒：等一会儿再试（一共约 1.6 秒）
    let mut delay = 20;
    for attempt in 0.. {
        match std::fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(_) if cfg!(windows) && attempt < 7 => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                delay = (delay * 2).min(500);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_wins() {
        let directory = std::env::temp_dir().join(format!("friday-core-writer-{}", std::process::id()));
        let path = directory.join("a.json");
        for index in 0..100 {
            write(path.clone(), move |file| write!(file, "{index}"));
        }
        flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "99");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn failed_render_keeps_the_old_file() {
        let directory = std::env::temp_dir().join(format!("friday-core-writer-failed-{}", std::process::id()));
        let path = directory.join("a.json");
        write_now(&path, b"old").unwrap();
        // 写了一半出错：原来的还在，临时文件不留
        write(path.clone(), |file| {
            file.write_all(b"half")?;
            Err(io::Error::other("编码出错"))
        });
        flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
