use std::{
    ffi::OsString,
    io,
    path::{self, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

const CONFIG_FILE: &str = "cargo-play.json";

pub fn resolve_home(home: Option<OsString>) -> Result<PathBuf> {
    let home = home
        .filter(|value| !value.is_empty())
        .context("请设置 CARGOPLAY_HOME，使其指向包含 play 子目录的根目录。")?;
    path::absolute(home).context("解析 CARGOPLAY_HOME 失败")
}

pub async fn prepare_config(home: &Path, create: bool) -> Result<()> {
    Config::read(home).await?.prepare(home, create).await
}

#[derive(Default, Deserialize, Serialize)]
pub struct Config {}

impl Config {
    async fn read(home: &Path) -> Result<Self> {
        let path = home.join(CONFIG_FILE);
        let data = match fs::read(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("读取 {} 失败", path.display()));
            }
        };
        let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&data);
        // A derived struct can also be read as a sequence by Serde.
        ensure!(
            data.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{'),
            "{CONFIG_FILE} 必须是 JSON 对象"
        );
        serde_json::from_slice(data).with_context(|| format!("{CONFIG_FILE} 必须是 JSON 对象"))
    }

    async fn prepare(&self, home: &Path, create: bool) -> Result<()> {
        if !create {
            return Ok(());
        }
        let path = home.join(CONFIG_FILE);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("创建 {} 失败", path.display()));
            }
        };
        let mut data = serde_json::to_vec_pretty(self)?;
        data.push(b'\n');
        file.write_all(&data)
            .await
            .with_context(|| format!("写入 {} 失败", path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("写入 {} 失败", path.display()))
    }
}
