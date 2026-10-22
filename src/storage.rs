use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

pub fn hash(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("无效保存路径")?;
    private_dir(parent)?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let operation = (|| -> Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if operation.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    operation
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec(value)?)
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    serde_json::from_slice(&fs::read(path)?).context("登录凭证文件格式错误")
}

pub fn lock(path: &Path) -> Result<File> {
    private_dir(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.join("sync.lock"))?;
    file.try_lock_exclusive()
        .context("已有同步进程运行，请勿重复启动")?;
    Ok(file)
}
