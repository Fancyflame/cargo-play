use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempPath};

use crate::{Result, repository::directory_exists};

const META_FILE: &str = "history-meta.json";

pub struct History {
    root: PathBuf,
    meta: PathBuf,
    entries: BTreeMap<String, Entry>,
}

#[derive(Deserialize, Serialize)]
struct Entry {
    file: String,
    archived_at: String,
}

impl History {
    pub fn open(home: &Path) -> Result<Self> {
        let root = home.join("history");
        if !directory_exists(&root)? {
            fs::create_dir(&root)
                .map_err(|error| format!("创建 {} 失败：{error}", root.display()))?;
        }
        let meta = home.join(META_FILE);
        let (entries, missing) = match fs::symlink_metadata(&meta) {
            Ok(info) if info.is_file() => {
                let data = fs::read(&meta)
                    .map_err(|error| format!("读取 {} 失败：{error}", meta.display()))?;
                let entries = serde_json::from_slice(&data)
                    .map_err(|error| format!("解析 {} 失败：{error}", meta.display()))?;
                (entries, false)
            }
            Ok(_) => return Err(format!("{} 必须是普通文件。", meta.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (BTreeMap::new(), true),
            Err(error) => return Err(format!("检查 {} 失败：{error}", meta.display())),
        };
        let history = Self {
            root,
            meta,
            entries,
        };
        if missing {
            history.write_index()?;
        }
        Ok(history)
    }

    pub fn next_package(&self, base: &str, current: Option<&str>) -> String {
        numbered(base)
            .find(|name| {
                !current.is_some_and(|current| same_filename(name, current))
                    && !self.entries.keys().any(|key| same_filename(name, key))
            })
            .expect("the sequence of project names is unbounded")
    }

    pub fn archive(
        &mut self,
        package: &str,
        create_bundle: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        let staging = tempfile::Builder::new()
            .prefix(".cargo-play-bundle-")
            .tempdir_in(&self.root)
            .map_err(|error| format!("创建归档临时目录失败：{error}"))?;
        let bundle = staging.path().join("archive.bundle");
        create_bundle(&bundle)?;
        let mut bundle = TempPath::try_from_path(bundle)
            .map_err(|error| format!("打开待发布的 Bundle 失败：{error}"))?;
        for stem in numbered(package) {
            if is_reserved(&stem) {
                continue;
            }
            let file = format!("{stem}.bundle");
            if self
                .entries
                .values()
                .any(|entry| same_filename(&file, &entry.file))
            {
                continue;
            }
            let path = self.root.join(&file);
            match bundle.persist_noclobber(&path) {
                Ok(()) => {
                    self.entries.insert(
                        package.into(),
                        Entry {
                            file,
                            archived_at: Timestamp::now()
                                .strftime("%Y-%m-%dT%H:%M:%SZ")
                                .to_string(),
                        },
                    );
                    if let Err(error) = self.write_index() {
                        return Err(match fs::remove_file(&path) {
                            Ok(()) => error,
                            Err(cleanup) => format!(
                                "{error}\n未登记的 Bundle 保留在 {}：{cleanup}",
                                path.display()
                            ),
                        });
                    }
                    return Ok(path);
                }
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    bundle = error.path;
                }
                Err(error) => {
                    return Err(format!(
                        "保存 Bundle {} 失败：{}",
                        path.display(),
                        error.error
                    ));
                }
            }
        }
        unreachable!("the sequence of bundle names is unbounded")
    }

    fn write_index(&self) -> Result<()> {
        let save = || -> io::Result<()> {
            let mut file = NamedTempFile::new_in(self.meta.parent().expect("index is under home"))?;
            serde_json::to_writer_pretty(&mut file, &self.entries)?;
            file.write_all(b"\n")?;
            file.as_file().sync_all()?;
            file.persist(&self.meta).map_err(|error| error.error)?;
            Ok(())
        };
        save().map_err(|error| format!("写入 {} 失败：{error}", self.meta.display()))
    }
}

fn numbered(base: &str) -> impl Iterator<Item = String> {
    std::iter::once(base.to_owned()).chain((2..).map(move |suffix| format!("{base}-{suffix}")))
}

fn same_filename(a: &str, b: &str) -> bool {
    a == b || (cfg!(windows) && a.eq_ignore_ascii_case(b))
}

fn is_reserved(stem: &str) -> bool {
    cfg!(windows) && {
        let stem = stem.to_ascii_uppercase();
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
    }
}

pub fn check_package_name(name: &str) -> Result<()> {
    // Cargo names are used as one filename component on both Unix and Windows.
    let path = Path::new(name);
    if name.is_empty()
        || path.file_name() != Some(OsStr::new(name))
        || name
            .chars()
            .any(|c| c.is_control() || ". /\\<>:\"|?*".contains(c))
    {
        return Err(format!("Cargo.toml 中的包名 {name:?} 无法用作归档文件名。"));
    }
    Ok(())
}
