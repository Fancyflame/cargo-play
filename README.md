# cargo-play (`cgpl`)

`cgpl` 是一个管理本地 Rust 试验的命令行工具。想试一个 crate、验证一段代码或复现一个小问题时，可以把项目建在固定的 `play/` 目录。切换试验时，`cgpl` 会把当前项目归档成 Git Bundle，下次可以按列表序号或 UUID 恢复。

## 安装

从源码构建需要 **Rust 1.89 或更新版本**。运行时需确保 `cargo` 和 `git` 都能从 `PATH` 找到。首次使用 Git 时，请先配置提交身份 `user.name` 和 `user.email`，因为 `cgpl new` 会创建初始提交。

在本仓库根目录执行以下命令。

```powershell
cargo install --path . --locked
cgpl --version
```

## 快速上手

以下命令使用 PowerShell。将 `CGPL_HOME` 指向专门存放试验的目录。这里设置的环境变量只在当前 PowerShell 会话及其子进程中生效。

```powershell
$env:CGPL_HOME = Join-Path $env:USERPROFILE 'cargo-playground'
cgpl init
cgpl new
```

`new` 会用当前 Cargo 的默认 edition 创建二进制项目，并尝试打开 `play/`。`init` 和 `new` 创建 Git 仓库时会采用 Git 的默认分支名，可通过 `init.defaultBranch` 配置。`--name` 设置列表中的显示名称，Cargo 包名仍由工具生成。修改 `src/main.rs` 后，可以运行项目并把改动提交到当前试验的本地 Git 历史。

```powershell
cargo run --manifest-path (Join-Path $env:CGPL_HOME 'play\Cargo.toml')
cgpl backup
```

在刚初始化的试验目录里，可以用下面的命令归档并恢复这个试验。`backup` 可以省略，归档会保存未提交的改动，包括未被 Git 忽略的未跟踪文件。

```powershell
cgpl clear
cgpl list
cgpl resume 1
```

也可以新建下一个试验并一次添加多个依赖。`new` 会先归档当前试验，再调用 `cargo add` 添加并提交依赖。

```powershell
cgpl new --name 'Serde demo' --deps serde@1 serde_json
```

## 命令速查

| 命令 | 用途 |
| --- | --- |
| `cgpl init` | 初始化配置、SQLite 数据库、`play/` 和 `history/`。 |
| `cgpl new [--name <名称>] [-d <依赖>...]` | 归档当前试验，创建新项目并打开目录。依赖支持 `crate` 或 `crate@版本`。 |
| `cgpl list [--offset <条数>]` | 按上次访问时间倒序列出试验，每次最多 50 条。用 `--offset 50` 查看第二页。 |
| `cgpl resume <序号或UUID>` | 归档当前试验，恢复并打开指定试验。 |
| `cgpl open` | 打开 `play/`。优先使用 VS Code，其次使用系统默认方式。 |
| `cgpl backup` | 把当前改动提交到本地 Git 仓库。Git Bundle 在归档时写入。 |
| `cgpl clear` | 归档当前试验并清空 `play/`。 |
| `cgpl delete [序号或UUID] [-y]` | 永久删除指定试验及其归档。省略目标时删除当前试验。默认交互确认，`-y` 或 `--yes` 跳过确认。 |

列表序号从 1 开始，翻页后继续使用全库序号。列表顺序可能随恢复或删除而变化，脚本中可使用 UUID 精确指定目标。运行 `cgpl <命令> --help` 可以查看选项。

> **清场前请检查需要保留的文件。** Bundle 包含当前分支的提交历史和未提交改动的快照。`target/` 等未被 Git 跟踪的忽略文件会随旧工作区一起清理。`new`、`resume` 和 `clear` 默认会保护未登记的工作区内容，确认可以丢弃这些内容后再使用 `--force`。

如果 `cargo init` 或 `cargo add` 在清场后失败，`play/` 可能已经清空。可以用 `cgpl list` 找到上一份仍保留的归档，再用 `cgpl resume` 恢复。

## 数据与自动清理

```text
CGPL_HOME/
├── cgpl_config.json5      # 配置
├── workspace.sqlite3     # 试验记录和当前试验状态
├── play/                 # 当前试验的工作目录
└── history/              # 归档文件 <UUID>.bundle
```

自动清理默认关闭。要启用它，编辑 `cgpl_config.json5`，按需填写以下限制。

```json5
{
  auto_delete: {
    after_count: 50,
    after_hours: 168,
    after_kbytes: 102400,
  },
}
```

- `after_count` 限制保留的归档数量。
- `after_hours` 限制距上次访问的小时数，支持小数。归档和恢复都会更新该试验的上次访问时间。
- `after_kbytes` 限制归档文件的总大小，单位为 KiB。示例中的 `102400` 等于 100 MiB。

`cgpl new` 在归档当前试验后执行自动清理，从上次访问时间最早的归档开始永久删除，直到满足所有已设置的限制。省略某个字段或将其设为 `null` 可取消对应限制。将 `auto_delete` 设为 `null` 可关闭自动清理。
