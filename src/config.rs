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

const CONFIG_FILE: &str = "cgpl_config.json5";
const LEGACY_CONFIG_FILE: &str = "cgpl_config.json";

pub fn resolve_home(home: Option<OsString>) -> Result<PathBuf> {
    let home = home
        .filter(|value| !value.is_empty())
        .context("请设置 CGPL_HOME，使其指向包含 play 子目录的根目录。")?;
    path::absolute(home).context("解析 CGPL_HOME 失败")
}

#[derive(Default, Deserialize, Serialize)]
pub struct Config {
    pub auto_delete: Option<AutoDeleteConfig>,
}

#[derive(Default, Deserialize, Serialize)]
pub struct AutoDeleteConfig {
    pub after_count: Option<u32>,
    pub after_hours: Option<f64>,
    pub after_kbytes: Option<u64>,
}

impl Config {
    pub async fn load(home: &Path, create: bool) -> Result<Self> {
        for filename in [CONFIG_FILE, LEGACY_CONFIG_FILE] {
            let path = home.join(filename);
            let Some(config) = Self::read(&path).await? else {
                continue;
            };
            if let Some(hours) = config
                .auto_delete
                .as_ref()
                .and_then(|limits| limits.after_hours)
            {
                ensure!(
                    hours.is_finite() && hours >= 0.0,
                    "{} 中的 auto_delete.after_hours 必须是非负有限值",
                    path.display()
                );
            }
            return Ok(config);
        }
        let config = Self::default();
        if create {
            config.write_if_missing(home).await?;
        }
        Ok(config)
    }

    async fn read(path: &Path) -> Result<Option<Self>> {
        let data = match fs::read_to_string(path).await {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("读取 {} 失败", path.display()));
            }
        };
        json5::from_str(&data)
            .map(Some)
            .with_context(|| format!("解析 JSON5 配置 {} 失败", path.display()))
    }

    async fn write_if_missing(&self, home: &Path) -> Result<()> {
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
        let mut data = json5::to_string(self)?;
        data.push('\n');
        file.write_all(data.as_bytes())
            .await
            .with_context(|| format!("写入 {} 失败", path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("写入 {} 失败", path.display()))
    }
}
