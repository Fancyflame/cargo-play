use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;
use jiff::{Timestamp, tz};
use serde_json::Value;
use tempfile::TempDir;

use crate::{Action, Cli, Playground, config::Config, default_package_name, history::History, run};

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
        let home = dir.path().join("Rust 试验场");
        let play = home.join("play");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("test-gitconfig"),
            "[user]\nname = Cargo Play Test\nemail = test@example.com\n[commit]\ngpgsign = false\n",
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
        Playground::open(
            Some(self.home.clone().into_os_string()),
            true,
            prepare_config,
        )
        .unwrap()
    }

    fn index(&self) -> Value {
        serde_json::from_slice(&fs::read(self.home.join("history-meta.json")).unwrap()).unwrap()
    }

    fn archive_at(&self, name: &str) -> PathBuf {
        self.home
            .join("history")
            .join(self.index()[name]["file"].as_str().unwrap())
    }

    fn run(
        &self,
        action: Action,
        open: impl FnOnce(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.with_git(|| {
            run(
                action,
                Some(self.home.clone().into_os_string()),
                prepare_config,
                open,
            )
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

fn prepare_config(home: &Path, create: bool) -> anyhow::Result<()> {
    Config::read(home)?.prepare(home, create)
}

fn new(deps: &[&str]) -> Action {
    Action::New {
        deps: deps.iter().map(|dep| (*dep).into()).collect(),
        force: false,
    }
}

fn not_opened(_: &Path) -> anyhow::Result<()> {
    panic!("不应打开编辑器");
}

#[test]
fn parses_cli() {
    let cli = Cli::try_parse_from(["cargo-play", "open"]).unwrap();
    assert!(matches!(cli.command, Action::Open));
    assert!(Cli::try_parse_from(["cargo-play", "new", "-d"]).is_err());
    assert!(Cli::try_parse_from(["cargo-play", "backup", "-d", "serde"]).is_err());
    let cli = Cli::try_parse_from(["cargo-play", "new", "-d", "serde@1", "anyhow"]).unwrap();
    assert!(
        matches!(cli.command, Action::New { deps, force: false } if deps == ["serde@1", "anyhow"])
    );
    for flag in ["-f", "--force"] {
        let cli = Cli::try_parse_from(["cargo-play", "new", "-d", "serde", flag]).unwrap();
        assert!(matches!(cli.command, Action::New { deps, force: true } if deps == ["serde"]));
    }
}

#[test]
fn opens_existing_folder_directly() {
    let fixture = Fixture::empty();
    fs::create_dir(&fixture.play).unwrap();
    let mut opened = false;
    fixture
        .run(Action::Open, |path| {
            assert_eq!(path, fixture.play);
            opened = true;
            Ok(())
        })
        .unwrap();
    assert!(opened);
    assert!(fs::read_dir(&fixture.play).unwrap().next().is_none());
    assert!(!fixture.home.join("cargo-play.json").exists());
}

#[test]
fn open_requires_an_existing_folder() {
    let fixture = Fixture::empty();
    let error = fixture.run(Action::Open, not_opened).unwrap_err();
    assert!(error.to_string().contains("找不到试验文件夹"));
    assert!(!fixture.play.exists());
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
    fixture.run(Action::Init, not_opened).unwrap();
    assert!(fixture.home.join("history").is_dir());
    assert_eq!(fixture.index(), serde_json::json!({}));
    assert!(!fixture.play.join("Cargo.toml").exists());
    fs::create_dir(fixture.home.join("target")).unwrap();
    fs::write(fixture.home.join("target/cache"), "keep").unwrap();
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
    assert!(!fixture.play.join("cargo-play.json").exists());
    assert_eq!(fixture.index(), serde_json::json!({}));
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}

#[cfg(windows)]
#[test]
fn new_from_play_directory_preserves_play() {
    const CHILD: &str = "CARGO_PLAY_TEST_FROM_PLAY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let play = std::env::current_dir().unwrap();
        let home = play.parent().unwrap();
        TEST_GIT_CONFIG.set(Some(home.join("test-gitconfig")));
        run(
            new(&[]),
            Some(home.as_os_str().to_owned()),
            prepare_config,
            |folder| {
                assert_eq!(folder, play);
                Ok(())
            },
        )
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
    assert!(format!("{error:#}").contains("Cargo.toml"));
    assert!(error.to_string().contains("--force"));
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound)
    );
    assert!(cache.is_file());

    let cli = Cli::try_parse_from(["cargo-play", "new", "-f"]).unwrap();
    fixture.run(cli.command, |_| Ok(())).unwrap();
    assert!(!cache.exists());
    assert!(fixture.play.join("Cargo.toml").is_file());
    assert_eq!(fixture.index(), serde_json::json!({}));
}

#[test]
fn invalid_manifest_can_be_discarded_only_after_new_project_is_ready() {
    let fixture = Fixture::previous_trial();
    fixture.vendor();
    fs::write(fixture.play.join("Cargo.toml"), "invalid toml").unwrap();
    let force = |deps: &[&str]| Action::New {
        deps: deps.iter().map(|dep| (*dep).into()).collect(),
        force: true,
    };

    let error = fixture
        .run(force(&["play-fixture-missing@1"]), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cargo add 失败"), "{error}");
    assert!(fixture.play.join("old.txt").is_file());
    assert_eq!(
        fs::read_to_string(fixture.play.join("Cargo.toml")).unwrap(),
        "invalid toml"
    );

    fixture.run(force(&[]), |_| Ok(())).unwrap();
    assert!(!fixture.play.join("old.txt").exists());
    assert_eq!(fixture.index(), serde_json::json!({}));
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}

#[test]
fn force_still_archives_a_project_with_a_valid_package_name() {
    let fixture = Fixture::previous_trial();
    fixture
        .run(
            Action::New {
                deps: vec![],
                force: true,
            },
            |_| Ok(()),
        )
        .unwrap();
    assert!(fixture.archive_at("old-trial").is_file());
}

#[test]
fn first_new_discards_legacy_projects_and_other_branches() {
    let fixture = Fixture::repository();
    fixture.write_trial();
    fixture.git(&["branch", "play/new-20260101-000000"]);
    fixture.run(Action::Init, not_opened).unwrap();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    assert_eq!(fixture.index(), serde_json::json!({}));
    assert!(
        fs::read_dir(fixture.home.join("history"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(!fixture.play.join("old.txt").exists());
    assert_eq!(
        fixture.git(&["for-each-ref", "--format=%(refname)", "refs/heads"]),
        "refs/heads/archive"
    );
}

#[test]
fn package_name_uses_local_time() {
    let instant: Timestamp = "2026-01-01T20:00:00Z".parse().unwrap();
    let local = instant.to_zoned(tz::offset(8).to_time_zone());
    assert_eq!(default_package_name(&local), "play-new-20260102-040000");
}

#[test]
fn new_archives_git_history_and_recreates_the_worktree() {
    let fixture = Fixture::previous_trial();
    let previous = fixture.git(&["rev-parse", "HEAD"]);
    // Use a TOML quoted key and literal string to exercise actual TOML parsing.
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

    let entry = &fixture.index()["renamed-trial"];
    assert_eq!(entry["file"], "renamed-trial.bundle");
    let timestamp = entry["archived_at"].as_str().unwrap();
    assert_eq!(
        timestamp.parse::<Timestamp>().unwrap().to_string(),
        timestamp
    );
    let bundle = fixture.archive_at("renamed-trial");
    fixture.git_at(
        &fixture.home,
        &["clone", bundle.to_str().unwrap(), "restored"],
    );
    let restored = fixture.home.join("restored");
    assert_eq!(fixture.git_at(&restored, &["rev-parse", "HEAD^"]), previous);
    assert!(
        fixture
            .git_at(&restored, &["log", "-1", "--format=%s"])
            .starts_with("backup: ")
    );
    assert_eq!(
        fs::read_to_string(restored.join("note.txt")).unwrap(),
        "latest"
    );
    for name in ["old.txt", "local.env", "target"] {
        assert!(!restored.join(name).exists());
    }
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
}

#[test]
fn consecutive_new_uses_unique_package_names() {
    let fixture = Fixture::empty();
    fixture.with_git(|| {
        let playground = fixture.playground();
        for expected in ["same-second", "same-second-2", "same-second-3"] {
            playground.new_project("same-second", &[], false).unwrap();
            let manifest = fs::read_to_string(fixture.play.join("Cargo.toml")).unwrap();
            assert_eq!(
                toml::from_str::<toml::Value>(&manifest).unwrap()["package"]["name"].as_str(),
                Some(expected)
            );
        }
    });
    assert!(fixture.archive_at("same-second").is_file());
    assert!(fixture.archive_at("same-second-2").is_file());
}

#[test]
fn same_package_name_preserves_existing_bundle_and_updates_index() {
    let fixture = Fixture::previous_trial();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let first = fixture.archive_at("old-trial");
    let bytes = fs::read(&first).unwrap();
    fs::write(fixture.play.join("Cargo.toml"), MANIFEST).unwrap();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    assert_eq!(fs::read(&first).unwrap(), bytes);
    assert_eq!(fixture.index()["old-trial"]["file"], "old-trial-2.bundle");
    assert!(fixture.archive_at("old-trial").is_file());
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
fn invalid_manifest_and_index_preserve_the_worktree() {
    let fixture = Fixture::previous_trial();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(
        fixture.play.join("Cargo.toml"),
        "[package]\nname = '../unsafe'",
    )
    .unwrap();
    assert!(
        fixture
            .run(new(&[]), not_opened)
            .unwrap_err()
            .chain()
            .any(|cause| cause.to_string().contains("归档文件名"))
    );
    fs::write(fixture.play.join("Cargo.toml"), "invalid toml").unwrap();
    assert!(
        fixture
            .run(new(&[]), not_opened)
            .unwrap_err()
            .chain()
            .any(|cause| cause.to_string().contains("[package].name"))
    );
    fs::write(
        fixture.play.join("Cargo.toml"),
        "[package]\nversion = '0.1.0'",
    )
    .unwrap();
    let error = fixture.run(new(&[]), not_opened).unwrap_err();
    assert!(
        format!("{error:#}").contains("[package].name") && error.to_string().contains("--force")
    );
    fs::write(fixture.play.join("Cargo.toml"), MANIFEST).unwrap();
    fs::write(fixture.home.join("history-meta.json"), "[]").unwrap();
    assert!(
        fixture
            .run(new(&[]), not_opened)
            .unwrap_err()
            .to_string()
            .contains("history-meta.json")
    );
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert!(fixture.play.join("old.txt").is_file());
}

#[test]
fn index_write_failure_removes_only_the_unpublished_archive() {
    let fixture = Fixture::previous_trial();
    let mut history = History::open(&fixture.home).unwrap();
    let index = fixture.home.join("history-meta.json");
    fs::rename(&index, index.with_extension("original")).unwrap();
    fs::create_dir(&index).unwrap();
    let error = history
        .archive("old-trial", |path| {
            fs::write(path, "test archive")?;
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("写入"), "{error}");
    assert!(!fixture.home.join("history/old-trial.bundle").exists());
    assert!(fixture.play.join("old.txt").is_file());
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
    let lock = fixture.playground();
    for action in [Action::Init, Action::Backup, new(&[])] {
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
            .run(new(&[]), not_opened)
            .unwrap_err()
            .to_string()
            .contains("备份失败")
    );
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  draft.txt");
    assert_eq!(fixture.index(), serde_json::json!({}));
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
    assert_eq!(fixture.index(), serde_json::json!({}));
    assert!(fixture.play.join("old.txt").is_file());
}

#[test]
fn cargo_failure_preserves_original_project_before_backup() {
    let fixture = Fixture::previous_trial();
    fixture.vendor();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    let error = fixture
        .run(new(&["play-fixture-missing@1"]), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("cargo add 失败"), "{error}");
    assert_eq!(fixture.git(&["log", "--format=%s"]), "old trial");
    assert_eq!(fixture.git(&["status", "--porcelain"]), "?? draft.txt");
    assert_eq!(fixture.index(), serde_json::json!({}));
}

#[test]
fn editor_failure_keeps_a_successful_trial() {
    let fixture = Fixture::empty();
    fixture
        .run(new(&[]), |_| anyhow::bail!("测试编辑器启动失败"))
        .unwrap();
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}
