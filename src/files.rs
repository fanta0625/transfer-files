//! Local-only copy engine. No overwrite, no link traversal, no source deletion.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
    pub link: bool,
    pub size: u64,
    pub modified: String,
}

pub fn read_directory(path: &Path) -> io::Result<(PathBuf, Vec<Entry>)> {
    let path = fs::canonicalize(path)?;
    let mut entries = Vec::new();
    for item in fs::read_dir(&path)? {
        let item = item?;
        let metadata = fs::symlink_metadata(item.path())?;
        let modified = metadata
            .modified()
            .ok()
            .map(|t| {
                chrono::DateTime::<chrono::Utc>::from(t)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|| "—".into());
        entries.push(Entry {
            path: item.path(),
            name: item.file_name().to_string_lossy().into_owned(),
            directory: metadata.is_dir(),
            link: metadata.file_type().is_symlink(),
            size: metadata.len(),
            modified,
        });
    }
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok((path, entries))
}

pub fn size_label(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub bytes: u64,
    pub total: u64,
    pub files: usize,
    pub total_files: usize,
    pub current: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub copied: usize,
    pub skipped: usize,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

#[derive(Debug)]
struct PlannedFile {
    source: PathBuf,
    destination: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Default)]
struct Plan {
    directories: Vec<PathBuf>,
    files: Vec<PlannedFile>,
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"))
    } else {
        Ok(())
    }
}

fn collect(
    source: &Path,
    destination: &Path,
    plan: &mut Plan,
    cancel: &AtomicBool,
) -> io::Result<()> {
    check_cancel(cancel)?;
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "原型不复制符号链接：{}",
            source.display()
        )));
    }
    if meta.is_dir() {
        plan.directories.push(destination.to_path_buf());
        for item in fs::read_dir(source)? {
            let item = item?;
            collect(
                &item.path(),
                &destination.join(item.file_name()),
                plan,
                cancel,
            )?;
        }
    } else if meta.is_file() {
        plan.files.push(PlannedFile {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    } else {
        return Err(io::Error::other(format!(
            "不支持的文件类型：{}",
            source.display()
        )));
    }
    Ok(())
}

// Always create temporary files on the destination filesystem. Hard-link publication
// atomically refuses an existing destination (rename would overwrite on Unix).
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn copy_file(
    file: &PlannedFile,
    progress: &mut Progress,
    cancel: &AtomicBool,
    notify: &mut impl FnMut(&Progress),
) -> io::Result<()> {
    check_cancel(cancel)?;
    let mut source = File::open(&file.source)?;
    let meta = source.metadata()?;
    if !meta.is_file() || meta.len() != file.size || meta.modified().ok() != file.modified {
        return Err(io::Error::other("源文件在扫描后发生变化，请重新发起复制"));
    }
    let parent = file
        .destination
        .parent()
        .ok_or_else(|| io::Error::other("无目标目录"))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = TemporaryFile(parent.join(format!(
        ".transfer-files-{}-{stamp}-{sequence}.part",
        std::process::id()
    )));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp.0)?;
    let mut buffer = vec![0; 256 * 1024];
    let mut written = 0;
    progress.current = file.source.display().to_string();
    notify(progress);
    loop {
        check_cancel(cancel)?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        written += count as u64;
        if written > file.size {
            return Err(io::Error::other("复制期间源文件大小发生变化"));
        }
        output.write_all(&buffer[..count])?;
        progress.bytes += count as u64;
        notify(progress);
    }
    let after = source.metadata()?;
    if written != file.size || after.len() != file.size || after.modified().ok() != file.modified {
        return Err(io::Error::other("复制期间源文件发生变化"));
    }
    output.sync_all()?;
    drop(output);
    check_cancel(cancel)?;
    fs::hard_link(&temp.0, &file.destination).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("无法发布目标文件（要求目标文件系统支持硬链接）：{e}"),
        )
    })?;
    progress.files += 1;
    notify(progress);
    Ok(())
}

/// Existing top-level names are skipped, including directories (no implicit merge).
/// Completed files remain after cancellation; only unfinished owned files are removed.
pub fn copy_batch(
    sources: &[PathBuf],
    target: &Path,
    cancel: &AtomicBool,
    mut notify: impl FnMut(&Progress),
) -> Report {
    let mut report = Report::default();
    let target = match fs::canonicalize(target) {
        Ok(p) if p.is_dir() => p,
        _ => {
            report.errors.push("目标目录不存在或无法访问".into());
            return report;
        }
    };
    let mut plan = Plan::default();
    let mut names = std::collections::HashSet::new();
    for source in sources {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            return report;
        }
        let result = (|| -> io::Result<()> {
            let original_meta = fs::symlink_metadata(source)?;
            if original_meta.file_type().is_symlink() {
                return Err(io::Error::other("不支持复制符号链接"));
            }
            let source = fs::canonicalize(source)?;
            let name = source
                .file_name()
                .ok_or_else(|| io::Error::other("不能复制文件系统根目录"))?;
            let destination = target.join(name);
            if source == destination || (source.is_dir() && target.starts_with(&source)) {
                return Err(io::Error::other("不能复制到自身或自身的子目录"));
            }
            if fs::symlink_metadata(&destination).is_ok() || !names.insert(name.to_os_string()) {
                report.skipped += 1;
                return Ok(());
            }
            let mut root_plan = Plan::default();
            collect(&source, &destination, &mut root_plan, cancel)?;
            plan.directories.extend(root_plan.directories);
            plan.files.extend(root_plan.files);
            Ok(())
        })();
        if let Err(error) = result {
            if error.kind() == io::ErrorKind::Interrupted {
                report.cancelled = true;
                return report;
            }
            report.errors.push(format!("{}：{error}", source.display()));
        }
    }
    let mut progress = Progress {
        total: plan.files.iter().map(|f| f.size).sum(),
        total_files: plan.files.len(),
        ..Progress::default()
    };
    notify(&progress);
    let mut created_directories = Vec::new();
    for directory in &plan.directories {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        match fs::create_dir(directory) {
            Ok(()) => created_directories.push(directory.clone()),
            Err(e) => {
                report
                    .errors
                    .push(format!("创建目录 {}：{e}", directory.display()));
                // Do not write into a directory that unexpectedly appeared after planning.
                for dir in created_directories.iter().rev() {
                    let _ = fs::remove_dir(dir);
                }
                return report;
            }
        }
    }
    if !report.cancelled {
        for file in &plan.files {
            match copy_file(file, &mut progress, cancel, &mut notify) {
                Ok(()) => report.copied += 1,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    report.cancelled = true;
                    break;
                }
                Err(e) => report
                    .errors
                    .push(format!("{}：{e}", file.destination.display())),
            }
        }
    }
    // Remove only empty directories on incomplete batches; keep completed output.
    if report.cancelled || !report.errors.is_empty() {
        for dir in created_directories.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn copies_nested_unicode_empty_files_and_empty_directories_without_overwriting() {
        let root = tempdir().unwrap();
        let src = root.path().join("资料 空格");
        let target = root.path().join("out");
        fs::create_dir_all(src.join("空目录")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(src.join("数据.txt"), "中文内容").unwrap();
        fs::write(src.join("empty"), []).unwrap();
        let cancel = AtomicBool::new(false);
        let report = copy_batch(std::slice::from_ref(&src), &target, &cancel, |_| {});
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.copied, 2);
        let output = target.join("资料 空格");
        assert_eq!(
            fs::read(output.join("数据.txt")).unwrap(),
            "中文内容".as_bytes()
        );
        assert!(output.join("空目录").is_dir());
        fs::write(src.join("数据.txt"), "changed").unwrap();
        let report = copy_batch(&[src], &target, &cancel, |_| {});
        assert_eq!(report.skipped, 1);
        assert_eq!(
            fs::read(output.join("数据.txt")).unwrap(),
            "中文内容".as_bytes()
        );
    }

    #[test]
    fn rejects_self_and_descendant_copy() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("child")).unwrap();
        let report = copy_batch(
            &[root.path().to_owned()],
            &root.path().join("child"),
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(report.errors.len(), 1);
        assert_eq!(fs::read_dir(root.path().join("child")).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_cleans_partial_file_and_preserves_source() {
        let root = tempdir().unwrap();
        let source = root.path().join("large.bin");
        fs::write(&source, vec![42; 1024 * 1024]).unwrap();
        let target = root.path().join("out");
        fs::create_dir(&target).unwrap();
        let cancel = AtomicBool::new(false);
        let report = copy_batch(std::slice::from_ref(&source), &target, &cancel, |p| {
            if p.bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(report.cancelled);
        assert!(source.exists());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn destination_created_during_copy_is_never_overwritten() {
        let root = tempdir().unwrap();
        let source = root.path().join("file.txt");
        fs::write(&source, "new").unwrap();
        let target = root.path().join("out");
        fs::create_dir(&target).unwrap();
        let destination = target.join("file.txt");
        let report = copy_batch(&[source], &target, &AtomicBool::new(false), |p| {
            if p.bytes > 0 && !destination.exists() {
                fs::write(&destination, "existing").unwrap();
            }
        });
        assert_eq!(fs::read_to_string(&destination).unwrap(), "existing");
        assert_eq!(report.errors.len(), 1);
        assert_eq!(fs::read_dir(target).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_tree_without_creating_output() {
        let root = tempdir().unwrap();
        let source = root.path().join("tree");
        let target = root.path().join("out");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(root.path(), source.join("loop")).unwrap();
        let report = copy_batch(&[source], &target, &AtomicBool::new(false), |_| {});
        assert!(!report.errors.is_empty());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }
}
