mod auto_delete;
mod deletion;
mod rename;

use std::{
    cell::RefCell,
    fs,
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;
use jiff::{Timestamp, tz};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    Action, Cli, Playground, PlaygroundTarget,
    config::Config,
    default_package_name, list_playgrounds, run,
    state::{NameMatch, PlaygroundRecord, State},
};

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

const MAIN: &str = "fn main() {}\n";
const MANIFEST: &str = "[package]\nname = \"old-trial\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

struct Fixture {
    _dir: TempDir,
    home: PathBuf,
    play: PathBuf,
}

impl Fixture {
    fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("Rust 演练场");
        let play = home.join("play");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("test-gitconfig"),
            "[user]\nname = Cargo Play Test\nemail = test@example.com\n[commit]\ngpgsign = false\n[init]\ndefaultBranch = sandbox-default\n",
        )
        .unwrap();
        Self {
            _dir: dir,
            home,
            play,
        }
    }

    fn repository() -> Self {
        let fixture = Self::empty();
        fs::create_dir(&fixture.play).unwrap();
        fixture.git(&["init", "--initial-branch=previous", "--template="]);
        for (key, value) in [
            ("user.name", "Cargo Play Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("core.hooksPath", ".git/no-hooks"),
        ] {
            fixture.git(&["config", key, value]);
        }
        fixture
    }

    fn previous_trial() -> Self {
        let fixture = Self::empty();
        fixture.run(Action::Init, not_opened).unwrap();
        fixture.write_trial();
        block_on(async {
            let state = State::open(&fixture.home).await.unwrap();
            let id = Uuid::new_v4();
            state
                .stage_new(id, "old-trial")
                .await
                .unwrap()
                .commit()
                .await
                .unwrap();
            fs::write(
                fixture.play.join(".git/cargo-play.bundle-worktree"),
                format!("{id}\n"),
            )
            .unwrap();
        });
        fixture
    }

    fn write_trial(&self) {
        fs::create_dir(self.play.join("src")).unwrap();
        fs::write(self.play.join("Cargo.toml"), MANIFEST).unwrap();
        fs::write(self.play.join("src/main.rs"), MAIN).unwrap();
        fs::write(self.play.join("old.txt"), "old").unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "old trial"]);
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_at(&self.play, args)
    }

    fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        let output = self.with_git(|| {
            configure_git(&mut command);
            command.current_dir(dir).args(args).output().unwrap()
        });
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn with_git<T>(&self, f: impl FnOnce() -> T) -> T {
        let prior = TEST_GIT_CONFIG.replace(Some(self.home.join("test-gitconfig")));
        let result = f();
        TEST_GIT_CONFIG.replace(prior);
        result
    }

    fn playground(&self) -> Playground {
        block_on(Playground::open(
            Some(self.home.clone().into_os_string()),
            true,
        ))
        .unwrap()
    }

    fn active(&self) -> PlaygroundRecord {
        block_on(async {
            State::open(&self.home)
                .await
                .unwrap()
                .active()
                .await
                .unwrap()
                .unwrap()
        })
    }

    fn record(&self, id: Uuid) -> PlaygroundRecord {
        block_on(async {
            State::open(&self.home)
                .await
                .unwrap()
                .by_id(id)
                .await
                .unwrap()
                .unwrap()
        })
    }

    fn lookup(&self, name: &str) -> NameMatch {
        block_on(async {
            State::open(&self.home)
                .await
                .unwrap()
                .find_by_name(name)
                .await
                .unwrap()
        })
    }

    fn archive_at(&self, name: &str) -> PathBuf {
        let NameMatch::Unique(id) = self.lookup(name) else {
            panic!("expected one match for {name:?}");
        };
        self.home.join(self.record(id).archived_path.unwrap())
    }

    fn assert_no_archives(&self) {
        assert_eq!(fs::read_dir(self.home.join("history")).unwrap().count(), 0);
    }

    fn run(
        &self,
        action: Action,
        open: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.with_git(|| {
            block_on(run(
                action,
                Some(self.home.clone().into_os_string()),
                async |path| open(path),
            ))
        })
    }

    fn fail_commits(&self) {
        self.git(&["config", "gpg.format", "openpgp"]);
        self.git(&[
            "config",
            "gpg.program",
            "nonexistent-cargo-play-test-gpg.exe",
        ]);
        self.git(&["config", "commit.gpgsign", "true"]);
    }

    // Cargo can resolve these fixture crates offline through its directory source.
    fn vendor(&self) {
        fs::create_dir_all(self.home.join(".cargo")).unwrap();
        fs::write(
            self.home.join(".cargo/config.toml"),
            r#"
[net]
offline = true
[source.crates-io]
replace-with = "fixtures"
[source.fixtures]
directory = "vendor"
"#,
        )
        .unwrap();
        for name in ["play-fixture-a", "play-fixture-b"] {
            let dir = self.home.join("vendor").join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2024\"\n"),
            )
            .unwrap();
            fs::write(dir.join("src/lib.rs"), "").unwrap();
            fs::write(
                dir.join(".cargo-checksum.json"),
                r#"{"files":{},"package":null}"#,
            )
            .unwrap();
        }
    }
}

thread_local! {
    static TEST_GIT_CONFIG: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) fn configure_git(command: &mut Command) {
    TEST_GIT_CONFIG.with_borrow(|config| {
        if let Some(config) = config {
            command
                .env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
    });
}

fn new(deps: &[&str]) -> Action {
    Action::New {
        deps: deps.iter().map(|dep| (*dep).into()).collect(),
        name: None,
        force: false,
    }
}

fn not_opened(_: &Path) -> anyhow::Result<()> {
    panic!("不应打开编辑器");
}

#[test]
fn parses_cli() {
    let cli = Cli::try_parse_from(["cgpl", "open"]).unwrap();
    assert!(matches!(
        cli.command,
        Action::Open {
            target: None,
            force: false
        }
    ));
    let cli = Cli::try_parse_from(["cgpl", "open", "12"]).unwrap();
    assert!(
        matches!(cli.command, Action::Open { target: Some(PlaygroundTarget::Position(position)), force: false } if position.get() == 12)
    );
    assert!(matches!(
        Cli::try_parse_from(["cgpl", "resume", "12"]),
        Err(error) if error.kind() == clap::error::ErrorKind::InvalidSubcommand
    ));
    let cli = Cli::try_parse_from(["cgpl", "clear"]).unwrap();
    assert!(matches!(cli.command, Action::Clear { force: false }));
    let cli = Cli::try_parse_from(["cgpl", "list"]).unwrap();
    assert!(matches!(cli.command, Action::List { offset: 0 }));
    let cli = Cli::try_parse_from(["cgpl", "list", "--offset", "50"]).unwrap();
    assert!(matches!(cli.command, Action::List { offset: 50 }));
    assert!(Cli::try_parse_from(["cgpl", "list", "--offset", "-1"]).is_err());
    assert!(Cli::try_parse_from(["cgpl", "new", "-d"]).is_err());
    assert!(Cli::try_parse_from(["cgpl", "backup", "-d", "serde"]).is_err());
    let cli = Cli::try_parse_from(["cgpl", "new", "-d", "serde@1", "anyhow"]).unwrap();
    assert!(
        matches!(cli.command, Action::New { deps, name: None, force: false } if deps == ["serde@1", "anyhow"])
    );
    let id = Uuid::new_v4();
    let uuid = id.to_string();
    let cli = Cli::try_parse_from(["cgpl", "open", &uuid]).unwrap();
    assert!(
        matches!(cli.command, Action::Open { target: Some(PlaygroundTarget::Id(parsed)), force: false } if parsed == id)
    );
    let cli = Cli::try_parse_from(["cgpl", "open", "10000", "--force"]).unwrap();
    assert!(
        matches!(cli.command, Action::Open { target: Some(PlaygroundTarget::Position(position)), force: true } if position.get() == 10000)
    );
    let cli = Cli::try_parse_from(["cgpl", "open", "00000000000000000000000000000001"]).unwrap();
    assert!(
        matches!(cli.command, Action::Open { target: Some(PlaygroundTarget::Id(id)), .. } if id == Uuid::from_u128(1))
    );
    for invalid in ["0", "-1", "1.5", "18446744073709551616", "not-a-uuid"] {
        assert!(
            Cli::try_parse_from(["cgpl", "open", invalid]).is_err(),
            "{invalid}"
        );
    }
    for flag in ["-f", "--force"] {
        assert!(
            matches!(Cli::try_parse_from(["cgpl", "open", flag]), Err(error) if error.kind() == clap::error::ErrorKind::MissingRequiredArgument)
        );
        let cli = Cli::try_parse_from(["cgpl", "clear", flag]).unwrap();
        assert!(matches!(cli.command, Action::Clear { force: true }));
        let cli = Cli::try_parse_from(["cgpl", "open", &uuid, flag]).unwrap();
        assert!(
            matches!(cli.command, Action::Open { target: Some(PlaygroundTarget::Id(parsed)), force: true } if parsed == id)
        );
        let cli = Cli::try_parse_from(["cgpl", "new", "-d", "serde", flag]).unwrap();
        assert!(
            matches!(cli.command, Action::New { deps, name: None, force: true } if deps == ["serde"])
        );
    }
}

async fn list_playgrounds_text(
    home: &Path,
    time_zone: &tz::TimeZone,
    offset: u64,
) -> anyhow::Result<String> {
    let mut table = list_playgrounds(home, time_zone, offset).await?;
    Ok(table.force_no_tty().to_string())
}

fn table_rows(table: &str) -> Vec<Vec<&str>> {
    table
        .lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| line.trim_matches('│').split('┆').map(str::trim).collect())
        .collect()
}

#[test]
fn lists_all_playgrounds_by_visit_time_and_uuid() {
    let fixture = Fixture::empty();
    let latest = Uuid::from_u128(4);
    let fraction = Uuid::from_u128(6);
    let tied_first = Uuid::from_u128(2);
    let tied_second = Uuid::from_u128(3);
    let oldest = Uuid::from_u128(1);
    let current = Uuid::from_u128(5);
    let table = block_on(async {
        let state = State::open(&fixture.home).await.unwrap();
        for (id, name, timestamp, archived_path) in [
            (
                tied_second,
                "重名",
                "2025-02-02T18:04:05Z",
                Some("history/three.bundle"),
            ),
            (
                latest,
                "重名",
                "2025-02-02T18:04:06Z",
                Some("history/four.bundle"),
            ),
            (
                fraction,
                "亚秒",
                "2025-02-02T18:04:05.000000001Z",
                Some("history/six.bundle"),
            ),
            (
                oldest,
                "旧演练场",
                "2025-02-01T18:04:05Z",
                Some("history/one.bundle"),
            ),
            (
                tied_first,
                "Rust",
                "2025-02-02T18:04:05Z",
                Some("history/two.bundle"),
            ),
            (current, "当前项目\n\x1b", "2025-02-02T18:04:05.5Z", None),
        ] {
            state.connection().execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO playgrounds (id, name, last_visit_time, archived_path) VALUES (?, ?, ?, ?)",
                [id.into(), name.into(), timestamp.into(), archived_path.into()],
            )).await.unwrap();
        }
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO current_playground (slot, playground_id) VALUES (1, ?)",
                [current.into()],
            ))
            .await
            .unwrap();
        let before = fs::read(fixture.home.join("workspace.sqlite3")).unwrap();
        let time_zone = tz::offset(8).to_time_zone();
        let table = list_playgrounds_text(&fixture.home, &time_zone, 0)
            .await
            .unwrap();
        assert_eq!(
            fs::read(fixture.home.join("workspace.sqlite3")).unwrap(),
            before
        );
        assert!(!fixture.home.join(".cargo-play.lock").exists());
        for (index, id) in [latest, current, fraction, tied_first, tied_second, oldest]
            .into_iter()
            .enumerate()
        {
            let position = NonZeroU64::new(index as u64 + 1).unwrap();
            assert_eq!(state.id_at_position(position).await.unwrap(), Some(id));
        }
        assert_eq!(State::list(&fixture.home, 1).await.unwrap()[0].id, current);
        table
    });
    assert_eq!(
        table_rows(&table),
        [
            ["序号", "名称", "ID", "上次访问"],
            [
                "1",
                "重名",
                &latest.to_string(),
                "2025-02-03 02:04:06 +08:00"
            ],
            [
                "2",
                r"当前项目\n\u{1b}",
                &current.to_string(),
                "2025-02-03 02:04:05 +08:00"
            ],
            [
                "3",
                "亚秒",
                &fraction.to_string(),
                "2025-02-03 02:04:05 +08:00"
            ],
            [
                "4",
                "Rust",
                &tied_first.to_string(),
                "2025-02-03 02:04:05 +08:00"
            ],
            [
                "5",
                "重名",
                &tied_second.to_string(),
                "2025-02-03 02:04:05 +08:00"
            ],
            [
                "6",
                "旧演练场",
                &oldest.to_string(),
                "2025-02-02 02:04:05 +08:00"
            ],
        ]
    );
    assert_eq!(
        table
            .lines()
            .filter_map(|line| line.chars().next())
            .collect::<String>(),
        "┌│╞│├│├│├│├│├│└"
    );
    assert!(!table.contains('\x1b'));
}

#[test]
fn list_wraps_cells_to_fit_narrow_terminals() {
    use unicode_width::UnicodeWidthStr;

    let fixture = Fixture::empty();
    let id = Uuid::from_u128(42);
    let name = "中文Rust演练场🦀".repeat(8);
    let mut table = block_on(async {
        let state = State::open(&fixture.home).await.unwrap();
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO playgrounds (id, name, last_visit_time) VALUES (?, ?, ?)",
                [
                    id.into(),
                    name.as_str().into(),
                    "2025-02-02T18:04:05Z".into(),
                ],
            ))
            .await
            .unwrap();
        list_playgrounds(&fixture.home, &tz::TimeZone::UTC, 0)
            .await
            .unwrap()
    });
    let full_width = table.force_no_tty().to_string();
    assert_eq!(table_rows(&full_width)[1][1], name);
    assert_eq!(table_rows(&full_width)[1][2], id.to_string());
    for width in [24, 32, 48, 80] {
        let wrapped = table.set_width(width).to_string();
        let line_width = wrapped.lines().next().unwrap().width();
        assert!(line_width <= usize::from(width), "{wrapped}");
        assert!(
            wrapped.lines().all(|line| line.width() == line_width),
            "{wrapped}"
        );
        assert!(wrapped.lines().count() > full_width.lines().count());
        assert!(!wrapped.lines().any(|line| line.starts_with('├')));
        let (_, body) = wrapped.split_once('╡').unwrap();
        let rows = table_rows(body);
        let name_parts: String = rows.iter().map(|row| row[1]).collect();
        let id_parts: String = rows.iter().map(|row| row[2]).collect();
        assert_eq!(name_parts, name);
        assert_eq!(id_parts, id.to_string());
    }
}

#[test]
fn lists_only_headers_for_an_empty_database() {
    let fixture = Fixture::empty();
    let table = block_on(async {
        State::open(&fixture.home).await.unwrap();
        list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, 0)
            .await
            .unwrap()
    });
    assert_eq!(table_rows(&table), [["序号", "名称", "ID", "上次访问"]]);
}

#[test]
fn list_requires_initialization_without_creating_state() {
    let fixture = Fixture::empty();
    let error = fixture
        .run(Action::List { offset: 0 }, not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cgpl init"));
    assert_eq!(fs::read_dir(&fixture.home).unwrap().count(), 1);
}

#[test]
fn list_reports_the_id_with_an_invalid_visit_timestamp() {
    let fixture = Fixture::empty();
    let id = Uuid::from_u128(1);
    let error = block_on(async {
        let state = State::open(&fixture.home).await.unwrap();
        state.connection().execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playgrounds (id, name, archived_path, last_visit_time) VALUES (?, '测试', 'history/old.bundle', 'invalid')",
            [id.into()],
        )).await.unwrap();
        list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, 0)
            .await
            .unwrap_err()
    });
    assert!(error.to_string().contains("上次访问时间无效"));
    assert!(error.to_string().contains(&id.to_string()));
}

#[test]
fn opens_existing_folder_directly() {
    let fixture = Fixture::empty();
    fs::create_dir(&fixture.play).unwrap();
    let mut opened = false;
    fixture
        .run(
            Action::Open {
                target: None,
                force: false,
            },
            |path| {
                assert_eq!(path, fixture.play);
                opened = true;
                Ok(())
            },
        )
        .unwrap();
    assert!(opened);
    assert!(fs::read_dir(&fixture.play).unwrap().next().is_none());
    assert!(!fixture.home.join("cgpl_config.json5").exists());
    assert!(!fixture.home.join("cgpl_config.json").exists());
}

#[test]
fn open_requires_an_existing_folder() {
    let fixture = Fixture::empty();
    let error = fixture
        .run(
            Action::Open {
                target: None,
                force: false,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(error.to_string().contains("找不到演练场文件夹"));
    assert!(!fixture.play.exists());
}

#[test]
fn loads_legacy_config_without_creating_json5() {
    let fixture = Fixture::empty();
    let path = fixture.home.join("cgpl_config.json");
    let original = r#"{"auto_delete": {"after_count": 3}}"#;
    fs::write(&path, original).unwrap();

    let config = block_on(Config::load(&fixture.home, true)).unwrap();

    assert_eq!(config.auto_delete.unwrap().after_count, Some(3));
    assert!(!fixture.home.join("cgpl_config.json5").exists());
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn init_and_first_new_create_history_and_clear_only_play() {
    let fixture = Fixture::empty();
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .to_string()
            .contains("首次使用")
    );
    fixture.run(Action::Init, not_opened).unwrap();
    assert_eq!(
        fixture.git(&["symbolic-ref", "--short", "HEAD"]),
        "sandbox-default"
    );
    fixture.run(Action::Init, not_opened).unwrap();
    assert!(fixture.home.join("cgpl_config.json5").is_file());
    assert!(!fixture.home.join("cgpl_config.json").exists());
    assert!(fixture.home.join("history").is_dir());
    fixture.assert_no_archives();
    assert!(!fixture.play.join("Cargo.toml").exists());
    fs::create_dir(fixture.home.join("target")).unwrap();
    fs::write(fixture.home.join("target/cache"), "keep").unwrap();
    fs::create_dir(fixture.play.join("target")).unwrap();
    fs::write(fixture.play.join("target/cache"), "disposable").unwrap();
    fixture
        .run(new(&[]), |path| {
            assert_eq!(path, fixture.play);
            assert!(
                fs::read_to_string(path.join("src/main.rs"))
                    .unwrap()
                    .contains("Hello, world!")
            );
            Ok(())
        })
        .unwrap();
    assert!(!fixture.home.join(".git").exists());
    assert!(fixture.home.join("target/cache").exists());
    assert!(!fixture.play.join("target").exists());
    assert!(!fixture.play.join("cgpl_config.json5").exists());
    fixture.assert_no_archives();
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
    assert_eq!(
        fixture.git(&["branch", "--show-current"]),
        "sandbox-default"
    );

    let reference = tempfile::tempdir().unwrap();
    let output = Command::new("cargo")
        .current_dir(reference.path())
        .args(["init", "--vcs", "none", "--bin", "--name", "reference", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let [generated, expected] = [fixture.play.as_path(), reference.path()].map(|project| {
        fs::read_to_string(project.join("Cargo.toml"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("edition = "))
            .unwrap()
            .to_owned()
    });
    assert_eq!(generated, expected);
}

#[cfg(windows)]
#[test]
fn new_from_play_directory_preserves_play() {
    const CHILD: &str = "CARGO_PLAY_TEST_FROM_PLAY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let play = std::env::current_dir().unwrap();
        let home = play.parent().unwrap();
        TEST_GIT_CONFIG.set(Some(home.join("test-gitconfig")));
        block_on(run(
            new(&[]),
            Some(home.as_os_str().to_owned()),
            async |folder| {
                assert_eq!(folder, play);
                Ok(())
            },
        ))
        .unwrap();
        assert_eq!(std::env::current_dir().unwrap(), play);
        assert!(Path::new("src/main.rs").is_file());
        return;
    }

    let fixture = Fixture::previous_trial();
    fs::write(fixture.play.join(".old"), "remove me").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::new_from_play_directory_preserves_play",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .current_dir(&fixture.play)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.play.join("old.txt").exists());
    assert!(!fixture.play.join(".old").exists());
    assert!(fixture.archive_at("old-trial").is_file());
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}

#[test]
fn nonempty_play_without_manifest_requires_force_even_without_git() {
    let fixture = Fixture::empty();
    fs::create_dir_all(fixture.play.join("target")).unwrap();
    let cache = fixture.play.join("target/cache");
    fs::write(&cache, "preserve until forced").unwrap();

    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(error.to_string().contains("未登记"));
    assert!(error.to_string().contains("--force"));
    assert!(cache.is_file());

    let cli = Cli::try_parse_from(["cgpl", "new", "-f"]).unwrap();
    fixture.run(cli.command, |_| Ok(())).unwrap();
    assert!(!cache.exists());
    assert!(fixture.play.join("Cargo.toml").is_file());
    fixture.assert_no_archives();
}

#[test]
fn invalid_manifest_is_archived_using_the_stored_name() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    fixture.vendor();
    fs::write(fixture.play.join("Cargo.toml"), "invalid toml").unwrap();
    let force = |deps: &[&str]| Action::New {
        deps: deps.iter().map(|dep| (*dep).into()).collect(),
        name: None,
        force: true,
    };

    let error = fixture
        .run(force(&["play-fixture-missing@1"]), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cargo add 失败"), "{error}");
    assert!(fs::read_dir(&fixture.play).unwrap().next().is_none());
    assert!(fixture.archive_at("old-trial").is_file());
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert!(fixture.play.join("old.txt").is_file());
    assert_eq!(
        fs::read_to_string(fixture.play.join("Cargo.toml")).unwrap(),
        "invalid toml"
    );

    fixture.run(new(&[]), |_| Ok(())).unwrap();
    assert!(!fixture.play.join("old.txt").exists());
    assert!(fixture.archive_at("old-trial").is_file());
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}

#[test]
fn force_still_archives_a_registered_project() {
    let fixture = Fixture::previous_trial();
    fixture
        .run(
            Action::New {
                deps: vec![],
                name: None,
                force: true,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert!(fixture.archive_at("old-trial").is_file());
}

#[test]
fn first_new_requires_force_to_discard_legacy_projects() {
    let fixture = Fixture::repository();
    fixture.write_trial();
    fixture.git(&["branch", "play/new-20260101-000000"]);
    fixture.run(Action::Init, not_opened).unwrap();
    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(error.to_string().contains("--force"));
    assert!(fixture.play.join("old.txt").is_file());
    let cli = Cli::try_parse_from(["cgpl", "new", "--force"]).unwrap();
    fixture.run(cli.command, |_| Ok(())).unwrap();
    fixture.assert_no_archives();
    assert!(!fixture.play.join("old.txt").exists());
    assert_eq!(
        fixture.git(&["for-each-ref", "--format=%(refname)", "refs/heads"]),
        "refs/heads/sandbox-default"
    );
}

#[test]
fn package_name_uses_local_time() {
    let instant: Timestamp = "2026-01-01T20:00:00Z".parse().unwrap();
    let local = instant.to_zoned_datetime(tz::offset(8).to_time_zone());
    assert_eq!(default_package_name(&local), "play-new-20260102-040000");
}

#[test]
fn new_archives_git_history_and_keeps_the_registered_name() {
    let fixture = Fixture::previous_trial();
    let previous = fixture.git(&["rev-parse", "HEAD"]);
    let id = fixture.active().id;
    // Changing the Cargo package name must not change the stored playground name.
    fs::write(
        fixture.play.join("Cargo.toml"),
        MANIFEST.replace("name = \"old-trial\"", "'name' = 'renamed-trial' # comment"),
    )
    .unwrap();
    fs::write(fixture.play.join(".gitignore"), "/local.env\n").unwrap();
    fs::write(fixture.play.join("local.env"), "discard").unwrap();
    fs::write(fixture.play.join("note.txt"), "latest").unwrap();
    fs::remove_file(fixture.play.join("old.txt")).unwrap();
    fs::create_dir(fixture.play.join("target")).unwrap();
    fs::write(fixture.play.join("target/cache"), "discard").unwrap();
    fixture.run(new(&[]), |_| Ok(())).unwrap();

    assert_eq!(fixture.lookup("renamed-trial"), NameMatch::Missing);
    assert_eq!(fixture.lookup("old-trial"), NameMatch::Unique(id));
    let entry = fixture.record(id);
    assert_eq!(entry.archived_path.unwrap(), format!("history/{id}.bundle"));
    let timestamp = &entry.last_visit_time;
    assert_eq!(
        timestamp.parse::<Timestamp>().unwrap().to_string(),
        *timestamp
    );
    let bundle = fixture.archive_at("old-trial");
    fixture.git_at(
        &fixture.home,
        &["clone", bundle.to_str().unwrap(), "restored"],
    );
    let restored = fixture.home.join("restored");
    assert_eq!(fixture.git_at(&restored, &["rev-parse", "HEAD"]), previous);
    assert!(!restored.join("note.txt").exists());
    for name in ["old.txt", "local.env", "target", "note.txt"] {
        assert!(!fixture.play.join(name).exists());
    }
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fixture.run(Action::Backup, not_opened).unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    fs::write(fixture.play.join("draft.txt"), "draft").unwrap();
    fixture.run(Action::Backup, not_opened).unwrap();
    assert_eq!(fixture.git(&["show", "HEAD:draft.txt"]), "draft");

    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), previous);
    assert_eq!(fixture.git(&["show", ":note.txt"]), "latest");
    assert_eq!(
        fs::read_to_string(fixture.play.join("note.txt")).unwrap(),
        "latest"
    );
    for name in ["old.txt", "local.env", "target"] {
        assert!(!fixture.play.join(name).exists());
    }
}

#[test]
fn consecutive_new_uses_unique_package_names() {
    let fixture = Fixture::empty();
    fixture.with_git(|| {
        let playground = fixture.playground();
        for expected in ["same-second", "same-second-2", "same-second-3"] {
            block_on(playground.new_project("same-second", None, &[], false)).unwrap();
            let manifest = fs::read_to_string(fixture.play.join("Cargo.toml")).unwrap();
            assert!(
                manifest
                    .lines()
                    .any(|line| line == format!("name = \"{expected}\""))
            );
            assert_eq!(fixture.active().name, expected);
        }
    });
    assert!(fixture.archive_at("same-second").is_file());
    assert!(fixture.archive_at("same-second-2").is_file());
}

#[test]
fn explicit_utf8_names_are_preserved_and_duplicate_names_require_ids() {
    let fixture = Fixture::empty();
    let name = "演练场 / \\ : * ? \" | 💡";
    let named = || {
        Cli::try_parse_from(["cgpl", "new", "--name", name])
            .unwrap()
            .command
    };
    fixture.run(named(), |_| Ok(())).unwrap();
    let first = fixture.active();
    assert_eq!(first.id.get_version_num(), 4);
    assert_eq!(first.name, name);
    assert!(first.last_visit_time.parse::<Timestamp>().is_ok());
    assert!(first.archived_path.is_none());
    assert_eq!(fixture.lookup(name), NameMatch::Unique(first.id));
    assert_eq!(fixture.lookup(&format!("{name} ")), NameMatch::Missing);
    fs::write(fixture.play.join("Cargo.toml"), MANIFEST).unwrap();
    fixture.run(named(), |_| Ok(())).unwrap();
    let second = fixture.active();
    assert_ne!(first.id, second.id);
    assert_eq!(fixture.lookup(name), NameMatch::Ambiguous);
    assert_eq!(fixture.record(first.id).name, name);
    assert_eq!(fixture.lookup("old-trial"), NameMatch::Missing);
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    for id in [first.id, second.id] {
        let path = fixture.record(id).archived_path.unwrap();
        assert_eq!(path, format!("history/{id}.bundle"));
        assert!(fixture.home.join(path).is_file());
    }
}

#[test]
fn backup_and_new_reject_tracked_target() {
    let fixture = Fixture::previous_trial();
    fs::create_dir(fixture.play.join("target")).unwrap();
    fs::write(fixture.play.join("target/keep.txt"), "keep").unwrap();
    fixture.git(&["add", "target"]);
    fixture.git(&["commit", "-m", "tracked build output"]);
    for action in [Action::Backup, new(&[])] {
        assert!(
            fixture
                .run(action, not_opened)
                .unwrap_err()
                .to_string()
                .contains("取消跟踪")
        );
    }
    assert!(fixture.play.join("target/keep.txt").is_file());
}

#[test]
fn invalid_database_preserves_the_worktree() {
    let fixture = Fixture::previous_trial();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(fixture.home.join("workspace.sqlite3"), "invalid database").unwrap();
    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(format!("{error:#}").contains("workspace.sqlite3"));
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert!(fixture.play.join("old.txt").is_file());
}

#[test]
fn blocked_archive_publication_restores_the_original_worktree() {
    let fixture = Fixture::previous_trial();
    let record = fixture.active();
    let destination = fixture.home.join(format!("history/{}.bundle", record.id));
    fs::create_dir(&destination).unwrap();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(fixture.play.join("old.txt"), "staged").unwrap();
    fixture.git(&["add", "old.txt"]);
    fs::write(fixture.play.join("old.txt"), "unstaged").unwrap();
    let index = fs::read(fixture.play.join(".git/index")).unwrap();
    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(format!("{error:#}").contains("归档路径"));
    assert!(destination.is_dir());
    assert_eq!(fixture.active(), record);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(fixture.play.join(".git/index")).unwrap(), index);
    assert_eq!(fixture.git(&["show", ":old.txt"]), "staged");
    assert_eq!(
        fs::read_to_string(fixture.play.join("old.txt")).unwrap(),
        "unstaged"
    );
    assert_eq!(fixture.git(&["for-each-ref", "refs/cargo-play"]), "");
    assert_eq!(fixture.git(&["stash", "list"]), "");
}

#[test]
fn rearchiving_the_same_uuid_overwrites_one_bundle_and_rolls_back_on_commit_failure() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    let bundle = fixture.home.join(format!("history/{id}.bundle"));
    // Simulate an active playground that already has an archive after resuming.
    let branch = fixture.git(&["symbolic-ref", "HEAD"]);
    fixture.git(&[
        "bundle",
        "create",
        bundle.to_str().unwrap(),
        "HEAD",
        &branch,
    ]);
    let bytes = fs::read(&bundle).unwrap();
    block_on(async {
        let state = State::open(&fixture.home).await.unwrap();
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE playgrounds SET archived_path = ?, last_visit_time = ? WHERE id = ?",
                [
                    format!("history/{id}.bundle").into(),
                    "2026-01-01T00:00:00Z".into(),
                    id.into(),
                ],
            ))
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE commit_failure (id INTEGER REFERENCES current_playground(slot) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER reject_commit AFTER UPDATE OF archived_path ON playgrounds BEGIN INSERT INTO commit_failure VALUES (2); END",
        ] {
            state
                .connection()
                .execute_unprepared(statement)
                .await
                .unwrap();
        }
    });
    let record = fixture.active();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    let index = fs::read(fixture.play.join(".git/index")).unwrap();
    fs::write(fixture.play.join("updated.txt"), "new contents").unwrap();
    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(format!("{error:#}").contains("提交工作区状态失败"));
    assert_eq!(fixture.active(), record);
    assert_eq!(fs::read(&bundle).unwrap(), bytes);
    assert_eq!(fs::read(fixture.play.join(".git/index")).unwrap(), index);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "?? updated.txt");
    assert_eq!(
        fs::read_dir(fixture.home.join("history")).unwrap().count(),
        1
    );

    block_on(async {
        let state = State::open(&fixture.home).await.unwrap();
        state
            .connection()
            .execute_unprepared("DROP TRIGGER reject_commit")
            .await
            .unwrap();
    });
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let updated = fixture.record(id);
    assert_eq!(updated.archived_path, record.archived_path);
    assert!(
        updated.last_visit_time.parse::<Timestamp>().unwrap()
            > record.last_visit_time.parse::<Timestamp>().unwrap()
    );
    assert_ne!(fs::read(&bundle).unwrap(), bytes);
    assert_eq!(
        fs::read_dir(fixture.home.join("history")).unwrap().count(),
        1
    );
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  updated.txt");
    assert_eq!(
        fs::read_to_string(fixture.play.join("updated.txt")).unwrap(),
        "new contents"
    );
}

#[test]
fn resumes_archived_projects_and_preserves_their_identity_and_history() {
    let fixture = Fixture::previous_trial();
    // Older versions used archive as the initial branch.
    fixture.git(&["branch", "-m", "archive"]);
    let original = fixture.active();
    let original_head = fixture.git(&["rev-parse", "HEAD"]);
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let other = fixture.active();
    let original_archive = fixture.record(original.id);
    assert!(
        original_archive
            .last_visit_time
            .parse::<Timestamp>()
            .unwrap()
            >= original.last_visit_time.parse::<Timestamp>().unwrap()
    );
    let original_bundle = fixture
        .home
        .join(original_archive.archived_path.as_ref().unwrap());
    let bundle_contents = fs::read(&original_bundle).unwrap();
    assert_eq!(
        fixture.git(&[
            "bundle",
            "list-heads",
            original_bundle.to_str().unwrap(),
            "refs/cargo-play/worktree"
        ]),
        ""
    );
    fs::write(fixture.play.join("draft.txt"), "other project changes").unwrap();

    let before_resume = Timestamp::now();
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(original.id)),
                force: true,
            },
            |path| {
                assert_eq!(path, fixture.play);
                Ok(())
            },
        )
        .unwrap();
    let resumed = fixture.active();
    assert_eq!(resumed.id, original_archive.id);
    assert_eq!(resumed.name, original_archive.name);
    assert_eq!(resumed.archived_path, original_archive.archived_path);
    let last_visit = resumed.last_visit_time.parse::<Timestamp>().unwrap();
    assert!((before_resume..=Timestamp::now()).contains(&last_visit));
    let other_visit = fixture
        .record(other.id)
        .last_visit_time
        .parse::<Timestamp>()
        .unwrap();
    assert!((before_resume..=last_visit).contains(&other_visit));
    assert_eq!(fixture.git(&["branch", "--show-current"]), "archive");
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), original_head);
    assert_eq!(fs::read(&original_bundle).unwrap(), bundle_contents);
    assert_eq!(
        fs::read_to_string(fixture.play.join(".git/cargo-play.bundle-worktree")).unwrap(),
        format!("{}\n", original.id)
    );
    let table = block_on(list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, 0)).unwrap();
    assert_eq!(
        table_rows(&table)[1],
        [
            "1",
            original.name.as_str(),
            &original.id.to_string(),
            last_visit
                .to_zoned_datetime(tz::TimeZone::UTC)
                .strftime("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
                .as_str(),
        ]
    );
    // The current playground still has archive metadata after a restore.
    assert_eq!(table_rows(&table).len(), 3);
    assert!(!fixture.play.join("draft.txt").exists());
    fs::write(fixture.play.join("old.txt"), "original project changes").unwrap();

    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(other.id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.active().id, other.id);
    assert_eq!(fixture.active().name, other.name);
    assert_eq!(
        fixture.git(&["branch", "--show-current"]),
        "sandbox-default"
    );
    assert_eq!(
        fs::read_to_string(fixture.play.join("draft.txt")).unwrap(),
        "other project changes"
    );
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  draft.txt");
    assert_ne!(fs::read(&original_bundle).unwrap(), bundle_contents);

    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(original.id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), original_head);
    assert_eq!(
        fs::read_to_string(fixture.play.join("old.txt")).unwrap(),
        "original project changes"
    );
    assert_eq!(
        fs::read_dir(fixture.home.join("history")).unwrap().count(),
        2
    );
}

#[test]
fn lists_pages_and_resumes_by_full_rank_beyond_first_page() {
    let fixture = Fixture::previous_trial();
    let original = fixture.active();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let current = fixture.active();
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE playgrounds SET last_visit_time = '2025-01-01T00:00:00Z' WHERE id = ?",
                [original.id.into()],
            ))
            .await
            .unwrap();
        state.connection().execute_unprepared(
            "WITH RECURSIVE positions(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM positions WHERE n < 10000)
             INSERT INTO playgrounds (id, name, archived_path, last_visit_time)
             SELECT CAST(printf('synthetic-%06d', n) AS BLOB), '归档', 'history/example.bundle',
                    '2025-01-02T00:00:00Z' FROM positions",
        ).await.unwrap();
        for offset in [0, 50] {
            let table = list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, offset)
                .await
                .unwrap();
            let rows = table_rows(&table);
            assert_eq!(rows.len(), 51);
            assert_eq!(rows[1][0], (offset + 1).to_string());
            assert_eq!(rows[50][0], (offset + 50).to_string());
            let first_id = if offset == 0 {
                current.id
            } else {
                Uuid::from_bytes(*b"synthetic-000050")
            };
            assert_eq!(rows[1][2], first_id.to_string());
        }
        let table = list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, 10000)
            .await
            .unwrap();
        let rows = table_rows(&table);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2][0], "10002");
        assert_eq!(rows[2][2], original.id.to_string());
        for offset in [10002, u64::MAX] {
            let table = list_playgrounds_text(&fixture.home, &tz::TimeZone::UTC, offset)
                .await
                .unwrap();
            assert_eq!(table_rows(&table), [["序号", "名称", "ID", "上次访问"]]);
        }
    });
    for position in ["10003", "18446744073709551615"] {
        let action = Cli::try_parse_from(["cgpl", "open", position])
            .unwrap()
            .command;
        let error = fixture.run(action, not_opened).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("找不到序号为 {position}"))
        );
        assert_eq!(fixture.active(), current);
    }
    let action = Cli::try_parse_from(["cgpl", "open", "10002"])
        .unwrap()
        .command;
    fixture
        .run(action, |path| {
            assert_eq!(path, fixture.play);
            Ok(())
        })
        .unwrap();
    assert_eq!(fixture.active().id, original.id);
    assert_eq!(
        fs::read_to_string(fixture.play.join("old.txt")).unwrap(),
        "old"
    );
}

#[test]
fn resuming_the_current_playground_updates_its_visit_time_and_opens_it() {
    let fixture = Fixture::previous_trial();
    let active = fixture.active();
    let marker = fixture.play.join(".git/cargo-play.bundle-worktree");
    fs::write(&marker, "invalid marker").unwrap();
    fs::write(fixture.play.join("draft.txt"), "still untracked").unwrap();
    for target in [
        PlaygroundTarget::Id(active.id),
        PlaygroundTarget::Position(NonZeroU64::MIN),
    ] {
        let mut opened = false;
        let before_resume = Timestamp::now();
        fixture
            .run(
                Action::Open {
                    target: Some(target),
                    force: false,
                },
                |path| {
                    assert_eq!(path, fixture.play);
                    opened = true;
                    Ok(())
                },
            )
            .unwrap();
        assert!(opened);
        let record = fixture.active();
        assert_eq!(record.id, active.id);
        let last_visit = record.last_visit_time.parse::<Timestamp>().unwrap();
        assert!((before_resume..=Timestamp::now()).contains(&last_visit));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "invalid marker");
    assert_eq!(fixture.git(&["status", "--porcelain"]), "?? draft.txt");
    fixture.assert_no_archives();
}

#[test]
fn clear_archives_pending_work_and_leaves_play_empty() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(fixture.play.join("old.txt"), "staged version").unwrap();
    fixture.git(&["add", "old.txt"]);
    fs::write(fixture.play.join("old.txt"), "updated").unwrap();
    fs::write(fixture.play.join("draft.txt"), "untracked draft").unwrap();

    fixture
        .run(Action::Clear { force: true }, not_opened)
        .unwrap();
    assert_eq!(fs::read_dir(&fixture.play).unwrap().count(), 0);
    assert!(block_on(async {
        State::open_existing(&fixture.home)
            .await
            .unwrap()
            .active()
            .await
            .unwrap()
            .is_none()
    }));
    let archive = fixture.archive_at("old-trial");
    assert!(archive.is_file());

    fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap();
    assert_eq!(fs::read_dir(&fixture.play).unwrap().count(), 0);
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.active().id, id);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["show", ":old.txt"]), "updated");
    assert_eq!(
        fs::read_to_string(fixture.play.join("old.txt")).unwrap(),
        "updated"
    );
    assert_eq!(
        fs::read_to_string(fixture.play.join("draft.txt")).unwrap(),
        "untracked draft"
    );
}

#[test]
fn clear_requires_force_to_discard_unregistered_contents() {
    let fixture = Fixture::empty();
    fixture.run(Action::Init, not_opened).unwrap();
    let unregistered = fixture.play.join("unregistered.txt");
    fs::write(&unregistered, "keep me").unwrap();
    let error = fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cgpl clear --force"));
    assert_eq!(fs::read_to_string(unregistered).unwrap(), "keep me");

    fixture
        .run(Action::Clear { force: true }, not_opened)
        .unwrap();
    assert_eq!(fs::read_dir(&fixture.play).unwrap().count(), 0);
    fixture.assert_no_archives();
}

#[test]
fn clear_restores_worktree_if_archive_cannot_be_published() {
    let fixture = Fixture::previous_trial();
    let current = fixture.active();
    let blocked = fixture.home.join(format!("history/{}.bundle", current.id));
    fs::create_dir(&blocked).unwrap();
    fs::write(fixture.play.join("draft.txt"), "still here").unwrap();

    let error = fixture
        .run(Action::Clear { force: true }, not_opened)
        .unwrap_err();
    assert!(format!("{error:#}").contains("归档路径"));
    assert!(blocked.is_dir());
    assert_eq!(fixture.active(), current);
    assert_eq!(
        fs::read_to_string(fixture.play.join("draft.txt")).unwrap(),
        "still here"
    );
}

#[test]
fn opening_a_target_requires_force_to_discard_an_unregistered_workspace() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    block_on(async {
        State::open(&fixture.home)
            .await
            .unwrap()
            .stage_clear(None, Timestamp::now())
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
    });
    fs::write(
        fixture.play.join("unregistered.txt"),
        "unregistered contents",
    )
    .unwrap();
    let error = fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("cgpl open {id} --force"))
    );
    assert!(fixture.play.join("unregistered.txt").is_file());

    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Position(NonZeroU64::new(2).unwrap())),
                force: true,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.active().id, id);
    assert!(!fixture.play.join("unregistered.txt").exists());
}

#[test]
fn unknown_uuid_does_not_clear_the_current_workspace() {
    let fixture = Fixture::previous_trial();
    let active = fixture.active();
    let id = Uuid::new_v4();
    let error = fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: true,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(error.to_string().contains(&id.to_string()));
    assert_eq!(fixture.active(), active);
    assert!(fixture.play.join("old.txt").is_file());
    fixture.assert_no_archives();
}

#[test]
fn failed_resume_leaves_the_previous_project_archived_and_recoverable() {
    let fixture = Fixture::previous_trial();
    let target = fixture.active();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let previous = fixture.active();
    let before_resume = fixture.record(target.id);
    fs::write(fixture.archive_at(&target.name), "invalid bundle").unwrap();
    fs::write(fixture.play.join("draft.txt"), "archive before cloning").unwrap();

    let error = fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(target.id)),
                force: false,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("git clone"), "{error:#}");
    assert_eq!(fixture.record(target.id), before_resume);
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        assert!(state.active().await.unwrap().is_none());
    });
    assert!(fixture.archive_at(&previous.name).is_file());
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(previous.id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.active().id, previous.id);
    assert_eq!(
        fs::read_to_string(fixture.play.join("draft.txt")).unwrap(),
        "archive before cloning"
    );
}

#[test]
fn failed_snapshot_apply_leaves_play_empty_and_the_previous_project_recoverable() {
    let fixture = Fixture::previous_trial();
    let target = fixture.active();
    let bundle = fixture.home.join(format!("history/{}.bundle", target.id));
    let backup = fixture.home.join("invalid-snapshot.bundle");
    let snapshot_ref = "refs/cargo-play/worktree";
    // Advertise an ordinary commit as the snapshot so cloning works but stash apply fails.
    let branch = fixture.git(&["symbolic-ref", "HEAD"]);
    fixture.git(&["update-ref", snapshot_ref, "HEAD"]);
    fixture.git(&[
        "bundle",
        "create",
        backup.to_str().unwrap(),
        "HEAD",
        &branch,
        snapshot_ref,
    ]);
    fixture.git(&["update-ref", "-d", snapshot_ref]);
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    fs::copy(&backup, &bundle).unwrap();
    let archived_target = fixture.record(target.id);
    let previous = fixture.active();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();

    let error = fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(target.id)),
                force: false,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("git stash"), "{error:#}");
    assert_eq!(fixture.record(target.id), archived_target);
    assert_eq!(fs::read_dir(&fixture.play).unwrap().count(), 0);
    block_on(async {
        assert!(
            State::open_existing(&fixture.home)
                .await
                .unwrap()
                .active()
                .await
                .unwrap()
                .is_none()
        );
    });
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(previous.id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.active().id, previous.id);
    assert_eq!(fixture.git(&["show", ":draft.txt"]), "keep");
}

#[test]
fn checks_git_operations() {
    let fixture = Fixture::previous_trial();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    let marker = fixture.play.join(".git/MERGE_HEAD");
    fs::write(&marker, format!("{head}\n")).unwrap();
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .to_string()
            .contains("MERGE_HEAD")
    );
    fs::remove_file(marker).unwrap();
    fixture.git(&["switch", "--detach"]);
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .to_string()
            .contains("分离 HEAD")
    );
}

#[test]
fn locks_mutating_commands_at_home() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    let lock = fixture.playground();
    for action in [
        Action::Init,
        Action::Backup,
        new(&[]),
        Action::Open {
            target: Some(PlaygroundTarget::Id(id)),
            force: false,
        },
        Action::Delete {
            target: None,
            yes: true,
        },
    ] {
        assert!(
            fixture
                .run(action, not_opened)
                .unwrap_err()
                .to_string()
                .contains("正在使用")
        );
    }
    drop(lock);
    fixture.run(Action::Backup, not_opened).unwrap();
}

#[test]
fn adds_multiple_dependencies_and_commits_them() {
    let fixture = Fixture::empty();
    fixture.vendor();
    fixture
        .run(new(&["play-fixture-a@1", "play-fixture-b@1"]), |_| Ok(()))
        .unwrap();
    assert_eq!(
        fixture.git(&["log", "--format=%s"]),
        "deps: add play-fixture-a@1 play-fixture-b@1\ninit: cargo init"
    );
    assert_eq!(
        fixture.git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]),
        "Cargo.lock\nCargo.toml"
    );
    assert!(
        fs::read_to_string(fixture.play.join("Cargo.lock"))
            .unwrap()
            .contains("play-fixture-b")
    );
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
}

#[test]
fn failed_backup_preserves_staged_work() {
    let fixture = Fixture::previous_trial();
    fixture.fail_commits();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .to_string()
            .contains("备份失败")
    );
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  draft.txt");
    fixture.assert_no_archives();
}

#[test]
fn clear_archives_untracked_files_without_running_a_commit() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fixture.fail_commits();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap();
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  draft.txt");
    assert_eq!(fixture.git(&["show", ":draft.txt"]), "keep");
}

#[test]
fn snapshot_ref_collision_preserves_the_existing_ref_and_worktree() {
    let fixture = Fixture::previous_trial();
    let record = fixture.active();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    let snapshot_ref = "refs/cargo-play/worktree";
    fixture.git(&["update-ref", snapshot_ref, &head]);
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    let error = fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap_err();
    assert!(format!("{error:#}").contains("创建临时归档引用失败"));
    assert_eq!(fixture.git(&["rev-parse", snapshot_ref]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "?? draft.txt");
    assert_eq!(fixture.active(), record);
    fixture.assert_no_archives();
}

#[test]
fn clear_requires_a_base_commit_for_the_snapshot() {
    let fixture = Fixture::previous_trial();
    let record = fixture.active();
    fixture.git(&["symbolic-ref", "HEAD", "refs/heads/unborn"]);
    let error = fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap_err();
    assert!(format!("{error:#}").contains("没有有效的 HEAD"));
    assert_eq!(fixture.git(&["symbolic-ref", "HEAD"]), "refs/heads/unborn");
    assert_eq!(fixture.active(), record);
    assert!(fixture.play.join("old.txt").is_file());
    fixture.assert_no_archives();
}

#[test]
fn bundle_failure_preserves_the_current_project() {
    let fixture = Fixture::previous_trial();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(fixture.play.join(".git/shallow"), format!("{head}\n")).unwrap();
    assert!(
        fixture
            .run(new(&[]), not_opened)
            .unwrap_err()
            .to_string()
            .contains("浅克隆")
    );
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    fixture.assert_no_archives();
    assert!(fixture.play.join("old.txt").is_file());
}

#[test]
fn cargo_failure_leaves_old_project_archived_and_play_empty() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    fixture.vendor();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    let error = fixture
        .run(new(&["play-fixture-missing@1"]), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cargo add 失败"), "{error}");
    assert!(fs::read_dir(&fixture.play).unwrap().next().is_none());
    assert!(fixture.archive_at("old-trial").is_file());
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.play.join("draft.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn editor_failure_keeps_a_successful_trial() {
    let fixture = Fixture::empty();
    fixture
        .run(new(&[]), |_| anyhow::bail!("测试编辑器启动失败"))
        .unwrap();
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}
