use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;
use jiff::{Timestamp, tz};
use tempfile::TempDir;

use crate::{Action, Cli, Repository, config::Config, default_branch_name, run};

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
        let fixture = Self::repository();
        fs::create_dir(fixture.play.join("src")).unwrap();
        fs::write(fixture.play.join("Cargo.toml"), MANIFEST).unwrap();
        fs::write(fixture.play.join("src/main.rs"), MAIN).unwrap();
        fs::write(fixture.play.join("old.txt"), "old").unwrap();
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-m", "old trial"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.play)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn run(
        &self,
        action: Action,
        open: impl FnOnce(&Path) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let repo = open_repo(
            Some(self.home.clone().into_os_string()),
            matches!(&action, Action::Init | Action::New { .. }),
        )?;
        run(action, repo, open)
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

fn open_repo(home: Option<OsString>, create: bool) -> crate::Result<Repository> {
    Repository::open(home, create, |home, create| {
        Config::read(home)?.prepare(home, create)
    })
}

fn new(deps: &[&str]) -> Action {
    Action::New {
        deps: deps.iter().map(|dep| (*dep).into()).collect(),
    }
}

fn not_opened(_: &Path) -> crate::Result<()> {
    panic!("不应打开编辑器");
}

#[test]
fn parses_cli() {
    assert!(Cli::try_parse_from(["cargo-play", "new", "-d"]).is_err());
    assert!(Cli::try_parse_from(["cargo-play", "backup", "-d", "serde"]).is_err());
    let cli = Cli::try_parse_from(["cargo-play", "new", "-d", "serde@1", "anyhow"]).unwrap();
    assert!(matches!(cli.command, Action::New { deps } if deps == ["serde@1", "anyhow"]));
}

#[test]
fn first_use_creates_nested_repository_and_config_and_preserves_targets() {
    let fixture = Fixture::empty();
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .contains("首次使用")
    );
    for target in [fixture.home.join("target"), fixture.play.join("target")] {
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "keep").unwrap();
    }
    // Bootstrap separately so the test can set a repository-local Git identity.
    fixture.run(Action::Init, not_opened).unwrap();
    fixture.run(Action::Init, not_opened).unwrap();
    assert_eq!(fixture.git(&["symbolic-ref", "--short", "HEAD"]), "archive");
    assert!(!fixture.play.join("Cargo.toml").exists());
    fixture.git(&["config", "user.name", "Cargo Play Test"]);
    fixture.git(&["config", "user.email", "test@example.com"]);
    fixture.git(&["config", "commit.gpgsign", "false"]);
    fixture
        .run(new(&[]), |path| {
            assert_eq!(path, fixture.play.join("src/main.rs"));
            assert!(fs::read_to_string(path).unwrap().contains("Hello, world!"));
            Ok(())
        })
        .unwrap();
    assert!(!fixture.home.join(".git").exists());
    assert!(fixture.play.join(".git").is_dir());
    assert_eq!(
        fs::read_to_string(fixture.home.join("cargo-play.json")).unwrap(),
        "{}\n"
    );
    assert!(!fixture.play.join("cargo-play.json").exists());
    for target in [fixture.home.join("target"), fixture.play.join("target")] {
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "keep");
    }
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
    assert!(fixture.git(&["ls-files", "--", "target"]).is_empty());
}

#[test]
fn branch_name_uses_local_time() {
    let instant: Timestamp = "2026-01-01T20:00:00Z".parse().unwrap();
    let local = instant.to_zoned(tz::offset(8).to_time_zone());
    assert_eq!(default_branch_name(&local), "play/new-20260102-040000");
}

#[test]
fn backs_up_old_project_then_initializes_fresh_trial() {
    let fixture = Fixture::previous_trial();
    let legacy_config = r#"{"template_branch":"missing"}"#;
    fs::write(fixture.home.join("cargo-play.json"), legacy_config).unwrap();
    fs::write(
        fixture.play.join("src/main.rs"),
        "fn main() { println!(\"old\"); }\n",
    )
    .unwrap();
    fixture.git(&["add", "src/main.rs"]);
    fs::write(fixture.play.join("note.txt"), "new file").unwrap();
    fs::remove_file(fixture.play.join("old.txt")).unwrap();
    fs::create_dir(fixture.play.join("target")).unwrap();
    fs::write(fixture.play.join("target/cache"), "existing build output").unwrap();

    fixture
        .run(new(&[]), |path| {
            assert!(fs::read_to_string(path).unwrap().contains("Hello, world!"));
            Ok(())
        })
        .unwrap();
    assert!(
        fixture
            .git(&["branch", "--show-current"])
            .starts_with("play/new-")
    );
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
    let message = fixture.git(&["log", "-1", "--format=%s", "previous"]);
    let timestamp = message.strip_prefix("backup: ").unwrap();
    assert_eq!(timestamp.len(), "2026-01-01T00:00:00Z".len());
    assert_eq!(
        timestamp.parse::<Timestamp>().unwrap().to_string(),
        timestamp
    );
    let files = fixture.git(&[
        "diff-tree",
        "--no-commit-id",
        "--name-status",
        "-r",
        "previous",
    ]);
    assert_eq!(files, "A\tnote.txt\nD\told.txt\nM\tsrc/main.rs");
    assert!(!fixture.play.join("note.txt").exists());
    assert!(fixture.play.join("target/cache").is_file());
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(fixture.home.join("cargo-play.json")).unwrap(),
        legacy_config
    );
    let before = fixture.git(&["rev-parse", "HEAD"]);
    fixture.run(Action::Backup, not_opened).unwrap();
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), before);
    fs::write(fixture.play.join("draft.txt"), "latest").unwrap();
    fixture.run(Action::Backup, not_opened).unwrap();
    assert_eq!(fixture.git(&["show", "HEAD:draft.txt"]), "latest");
}

#[test]
fn refuses_to_remove_tracked_target_content() {
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
                .contains("取消跟踪")
        );
    }
    assert_eq!(fixture.git(&["branch", "--show-current"]), "previous");
    assert_eq!(
        fs::read_to_string(fixture.play.join("target/keep.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn rejects_invalid_configuration_before_backup() {
    assert!(
        open_repo(None, false)
            .map(|_| ())
            .unwrap_err()
            .contains("CARGOPLAY_HOME")
    );
    let fixture = Fixture::previous_trial();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    fs::write(fixture.play.join("draft.txt"), "not ready").unwrap();
    for invalid in ["{", "[]"] {
        fs::write(fixture.home.join("cargo-play.json"), invalid).unwrap();
        assert!(
            fixture
                .run(new(&[]), not_opened)
                .unwrap_err()
                .contains("必须是 JSON 对象")
        );
    }
    assert_eq!(fixture.git(&["status", "--porcelain"]), "?? draft.txt");
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
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
            .contains("MERGE_HEAD")
    );
    fs::remove_file(marker).unwrap();
    fixture.git(&["switch", "--detach"]);
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .contains("分离 HEAD")
    );
}

#[test]
fn branch_suffix_and_worktree_lock() {
    let fixture = Fixture::previous_trial();
    let base = "play/20260101-000000Z";
    fixture.git(&["branch", base]);
    fixture.git(&["branch", &format!("{base}-2")]);
    let repo = open_repo(Some(fixture.home.clone().into_os_string()), false).unwrap();
    assert!(
        fixture
            .run(Action::Backup, not_opened)
            .unwrap_err()
            .contains("正在使用")
    );
    assert_eq!(repo.new_branch(base).unwrap(), format!("{base}-3"));
    drop(repo);
    fixture.run(Action::Backup, not_opened).unwrap();
}

#[test]
fn adds_multiple_dependencies_and_records_noop_dependency_commit() {
    let fixture = Fixture::repository();
    fixture.vendor();
    let deps = &["play-fixture-a@1", "play-fixture-b@1"];
    fixture.run(new(deps), |_| Ok(())).unwrap();
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

    let repo = open_repo(Some(fixture.home.clone().into_os_string()), false).unwrap();
    repo.add_dependencies(&deps.iter().map(|dep| (*dep).into()).collect::<Vec<_>>())
        .unwrap();
    assert_eq!(
        fixture.git(&["rev-parse", "HEAD^{tree}"]),
        fixture.git(&["rev-parse", "HEAD^^{tree}"])
    );
}

#[test]
fn failed_backup_stays_on_old_branch() {
    let fixture = Fixture::previous_trial();
    fixture.fail_commits();
    fs::write(fixture.play.join("draft.txt"), "keep").unwrap();
    assert!(
        fixture
            .run(new(&[]), not_opened)
            .unwrap_err()
            .contains("备份失败")
    );
    assert_eq!(fixture.git(&["branch", "--show-current"]), "previous");
    assert_eq!(fixture.git(&["status", "--porcelain"]), "A  draft.txt");
}

#[test]
fn cargo_failure_preserves_initialized_branch() {
    let fixture = Fixture::repository();
    fixture.vendor();
    let error = fixture
        .run(new(&["play-fixture-missing@1"]), not_opened)
        .unwrap_err();
    assert!(error.contains("cargo add 失败"), "{error}");
    assert!(error.contains("cargo-play backup"));
    assert!(
        fixture
            .git(&["branch", "--show-current"])
            .starts_with("play/")
    );
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}

#[test]
fn dependency_commit_failure_preserves_staged_changes() {
    let fixture = Fixture::repository();
    fixture.vendor();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    fixture.fail_commits();
    let repo = open_repo(Some(fixture.home.clone().into_os_string()), false).unwrap();
    let error = repo
        .add_dependencies(&["play-fixture-a@1".into()])
        .unwrap_err();
    assert!(error.contains("依赖提交失败"), "{error}");
    assert_eq!(
        fixture.git(&["status", "--porcelain"]),
        "A  Cargo.lock\nM  Cargo.toml"
    );
}

#[test]
fn editor_failure_keeps_a_successful_trial() {
    let fixture = Fixture::repository();
    fixture
        .run(new(&[]), |_| Err("测试编辑器启动失败".into()))
        .unwrap();
    assert!(
        fixture
            .git(&["branch", "--show-current"])
            .starts_with("play/")
    );
    assert_eq!(fixture.git(&["log", "--format=%s"]), "init: cargo init");
}
