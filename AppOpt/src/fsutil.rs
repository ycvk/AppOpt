use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 写同目录临时文件后 rename 覆盖目标，读者只会看到完整内容
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}
