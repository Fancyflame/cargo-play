use super::{
    Action, Cli, DbBackend, Fixture, PlaygroundRecord, PlaygroundTarget, State, Statement, Uuid,
    block_on, fs, new, not_opened,
};
use crate::read_delete_confirmation;
use clap::Parser;
use sea_orm::ConnectionTrait;

#[test]
fn parses_delete_targets_and_yes_flags() {
    let id = Uuid::new_v4();
    let cli = Cli::try_parse_from(["cgpl", "delete"]).unwrap();
    assert!(matches!(
        cli.command,
        Action::Delete {
            target: None,
            yes: false
        }
    ));
    let cli = Cli::try_parse_from(["cgpl", "delete", "-y"]).unwrap();
    assert!(matches!(
        cli.command,
        Action::Delete {
            target: None,
            yes: true
        }
    ));
    let cli = Cli::try_parse_from(["cgpl", "delete", &id.to_string(), "-y"]).unwrap();
    assert!(matches!(cli.command,
        Action::Delete { target: Some(PlaygroundTarget::Id(parsed)), yes: true } if parsed == id
    ));
    let cli = Cli::try_parse_from(["cgpl", "delete", "51", "--yes"]).unwrap();
    assert!(matches!(cli.command,
        Action::Delete { target: Some(PlaygroundTarget::Position(position)), yes: true }
        if position.get() == 51
    ));
    for input in ["0", "-1", "not-a-uuid"] {
        assert!(Cli::try_parse_from(["cgpl", "delete", input]).is_err());
    }
}

#[test]
fn confirmation_requires_an_explicit_yes_and_can_cancel_deletion() {
    for (input, confirmed) in [
        ("y\n", true),
        (" YES \r\n", true),
        ("n\n", false),
        ("yes please\n", false),
        ("\n", false),
        ("", false),
    ] {
        assert_eq!(
            read_delete_confirmation(input.as_bytes()).unwrap(),
            confirmed
        );
    }
    let fixture = Fixture::previous_trial();
    let record = fixture.active();
    let playground = fixture.playground();
    block_on(playground.delete(None, |target, is_current| {
        assert_eq!(target, &record);
        assert!(is_current);
        Ok(false)
    }))
    .unwrap();
    assert_eq!(fixture.active(), record);
    assert!(fixture.play.join("old.txt").is_file());
}

#[test]
fn deletes_archived_playground_by_global_rank() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let current = fixture.active();
    let bundle = fixture.home.join(fixture.record(id).archived_path.unwrap());
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE playgrounds SET last_visit_time = '2000-01-01T00:00:00Z' WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap();
        state.connection().execute_unprepared(
            "WITH RECURSIVE positions(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM positions WHERE n < 50)
             INSERT INTO playgrounds (id, name, last_visit_time)
             SELECT CAST(printf('synthetic-%06d', n) AS BLOB), '归档', '2000-01-02T00:00:00Z'
             FROM positions",
        ).await.unwrap();
        assert_eq!(State::list(&fixture.home, 51).await.unwrap()[0].id, id);
    });
    let action = Cli::try_parse_from(["cgpl", "delete", "52", "--yes"])
        .unwrap()
        .command;
    fixture.run(action, not_opened).unwrap();
    assert!(!bundle.exists());
    assert_eq!(fixture.active(), current);
    assert!(fixture.play.join("Cargo.toml").is_file());
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        assert!(state.by_id(id).await.unwrap().is_none());
        assert!(State::list(&fixture.home, 51).await.unwrap().is_empty());
    });
    for target in ["52", &id.to_string()] {
        let action = Cli::try_parse_from(["cgpl", "delete", target, "-y"])
            .unwrap()
            .command;
        assert!(
            fixture
                .run(action, not_opened)
                .unwrap_err()
                .to_string()
                .contains("找不到")
        );
    }
    assert_eq!(fixture.active(), current);
}

#[test]
fn deletes_the_current_playground_and_its_old_bundle_without_a_target() {
    let fixture = Fixture::previous_trial();
    let original = fixture.active();
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let other = fixture.active();
    fixture
        .run(
            Action::Open {
                target: Some(PlaygroundTarget::Id(original.id)),
                force: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    let bundle = fixture
        .home
        .join(fixture.record(original.id).archived_path.unwrap());
    fs::write(fixture.play.join("old.txt"), "uncommitted").unwrap();
    fs::write(fixture.play.join("untracked.txt"), "untracked").unwrap();
    fs::create_dir(fixture.play.join("target")).unwrap();
    fs::write(fixture.play.join("target/artifact"), "ignored").unwrap();
    let action = Cli::try_parse_from(["cgpl", "delete", "-y"])
        .unwrap()
        .command;
    fixture.run(action, not_opened).unwrap();
    assert!(fixture.play.is_dir());
    assert_eq!(fs::read_dir(&fixture.play).unwrap().count(), 0);
    assert!(!bundle.exists());
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        assert!(state.active().await.unwrap().is_none());
        assert!(state.by_id(original.id).await.unwrap().is_none());
    });
    let error = fixture
        .run(
            Action::Delete {
                target: None,
                yes: true,
            },
            not_opened,
        )
        .unwrap_err();
    assert!(error.to_string().contains("没有当前游乐场"));
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
}

fn update_bundle_path(fixture: &Fixture, id: Uuid, path: &str) {
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        state
            .connection()
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE playgrounds SET archived_path = ? WHERE id = ?",
                [path.into(), id.into()],
            ))
            .await
            .unwrap();
    });
}

fn delete_by_id(id: Uuid) -> Action {
    Action::Delete {
        target: Some(PlaygroundTarget::Id(id)),
        yes: true,
    }
}

#[test]
fn rejects_mismatched_archive_paths_and_accepts_missing_bundles() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let current = fixture.active();
    let path = fixture.record(id).archived_path.unwrap();
    let bundle = fixture.home.join(&path);
    update_bundle_path(&fixture, id, "play/Cargo.toml");
    let error = fixture.run(delete_by_id(id), not_opened).unwrap_err();
    assert!(error.to_string().contains("归档路径不匹配"));
    assert_eq!(
        fixture.record(id).archived_path.as_deref(),
        Some("play/Cargo.toml")
    );
    assert!(bundle.is_file());
    assert!(fixture.play.join("Cargo.toml").is_file());
    update_bundle_path(&fixture, id, &path);
    fs::remove_file(&bundle).unwrap();
    fixture.run(delete_by_id(id), not_opened).unwrap();
    assert_eq!(fixture.active(), current);
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        assert!(state.by_id(id).await.unwrap().is_none());
    });
}

#[test]
fn refuses_to_delete_the_current_worktree_with_a_mismatched_uuid() {
    let fixture = Fixture::previous_trial();
    let record = fixture.active();
    fs::write(
        fixture.play.join(".git/cargo-play.bundle-worktree"),
        Uuid::new_v4().to_string(),
    )
    .unwrap();
    let error = fixture
        .run(delete_by_id(record.id), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("不一致"));
    assert_eq!(fixture.active(), record);
    assert!(fixture.play.join("old.txt").is_file());
}

// Simulate an active playground that still has a bundle from an earlier archive.
fn active_with_bundle() -> (Fixture, PlaygroundRecord, std::path::PathBuf) {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    let path = format!("history/{id}.bundle");
    let bundle = fixture.home.join(&path);
    let branch = fixture.git(&["symbolic-ref", "HEAD"]);
    fixture.git(&[
        "bundle",
        "create",
        bundle.to_str().unwrap(),
        "HEAD",
        &branch,
    ]);
    update_bundle_path(&fixture, id, &path);
    let record = fixture.active();
    (fixture, record, bundle)
}

#[test]
fn restores_files_and_database_state_if_delete_commit_fails() {
    let (fixture, record, bundle) = active_with_bundle();
    let bytes = fs::read(&bundle).unwrap();
    fs::write(fixture.play.join("uncommitted.txt"), "uncommitted").unwrap();
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        for sql in [
            "CREATE TABLE commit_failure (id BLOB REFERENCES playgrounds(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER reject_delete AFTER DELETE ON playgrounds BEGIN INSERT INTO commit_failure VALUES (OLD.id); END",
        ] {
            state.connection().execute_unprepared(sql).await.unwrap();
        }
    });
    let error = fixture
        .run(delete_by_id(record.id), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("提交游乐场删除事务失败"));
    assert_eq!(fixture.active(), record);
    assert_eq!(fs::read(bundle).unwrap(), bytes);
    assert_eq!(
        fs::read_to_string(fixture.play.join("uncommitted.txt")).unwrap(),
        "uncommitted"
    );
    fixture.run(Action::Backup, not_opened).unwrap();
}

#[cfg(windows)]
#[test]
fn restores_a_staged_bundle_if_a_worktree_file_cannot_be_moved() {
    use std::os::windows::fs::OpenOptionsExt;

    let (fixture, record, bundle) = active_with_bundle();
    let bytes = fs::read(&bundle).unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(fixture.play.join("old.txt"))
        .unwrap();
    let error = fixture
        .run(delete_by_id(record.id), not_opened)
        .unwrap_err();
    assert!(error.to_string().contains("移开原工作区内容失败"));
    drop(locked);
    assert_eq!(fixture.active(), record);
    assert_eq!(fs::read(bundle).unwrap(), bytes);
    assert!(fixture.play.join("old.txt").is_file());
    assert!(
        fixture
            .play
            .join(".git/cargo-play.bundle-worktree")
            .is_file()
    );
}
