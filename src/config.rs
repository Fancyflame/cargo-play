use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{self, Path, PathBuf},
    sync::LazyLock,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "cargo-play.json";

pub static CONFIG: LazyLock<Result<Config>> = LazyLock::new(|| {
    let home = resolve_home(env::var_os("CARGOPLAY_HOME"))?;
    Config::read(&home)
});

pub fn resolve_home(home: Option<OsString>) -> Result<PathBuf> {
    let home = home
        .filter(|value| !value.is_empty())
        .context("请设置 CARGOPLAY_HOME，使其指向包含 play 子目录的根目录。")?;
    path::absolute(home).context("解析 CARGOPLAY_HOME 失败")
}

pub fn prepare_global(home: &Path, create: bool) -> Result<()> {
    let config = LazyLock::force(&CONFIG).as_ref().map_err(|error| {
        let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
        anyhow::Error::new(source)
    })?;
    config.prepare(home, create)
}

#[derive(Default, Deserialize, Serialize)]
pub struct Config {}

impl Config {
    pub fn read(home: &Path) -> Result<Self> {
        let path = home.join(CONFIG_FILE);
        let data = match fs::read(&path) {
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

    pub fn prepare(&self, home: &Path, create: bool) -> Result<()> {
        if !create {
            return Ok(());
        }
        let path = home.join(CONFIG_FILE);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("创建 {} 失败", path.display()));
            }
        };
        serde_json::to_writer_pretty(&mut file, self)
            .with_context(|| format!("写入 {} 失败", path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("写入 {} 失败", path.display()))
    }
}
