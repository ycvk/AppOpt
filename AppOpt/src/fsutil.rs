use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 写同目录临时文件后 rename 覆盖目标，读者只会看到完整内容
///
/// 目标目录可能由他人控制（规则文件路径可经 web 修改）：临时文件名上的残留项先删除，
/// 再以 create_new 新建，不会跟随符号链接写到别处；被抢占时报错
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        // 尽力清理，返回的仍是原始错误
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planted_temp_symlink_is_not_followed() {
        let dir = std::env::temp_dir().join(format!("appopt-fsutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, "keep").unwrap();
        let target = dir.join("a.conf");
        std::os::unix::fs::symlink(&victim, dir.join("a.conf.tmp")).unwrap();

        write_atomic(&target, b"new").unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(
            !fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(dir.join("a.conf.tmp")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
