use super::{
    Action, DbBackend, Fixture, State, Statement, Timestamp, Uuid, block_on, fs, new, not_opened,
};
use sea_orm::ConnectionTrait;
use serde_json::{Value, json};
use std::path::PathBuf;

fn configure(fixture: &Fixture, limits: Value) {
    fs::write(
        fixture.home.join("cgpl_config.json"),
        json!({ "auto_delete": limits }).to_string(),
    )
    .unwrap();
}

fn seed_archive(fixture: &Fixture, id: Uuid, visited: &str, bytes: Option<usize>) -> PathBuf {
    let relative_path = format!("history/{id}.bundle");
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        state.connection().execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playgrounds (id, name, archived_path, last_visit_time) VALUES (?, 'archive', ?, ?)",
            [id.into(), relative_path.clone().into(), visited.into()],
        )).await.unwrap();
    });
    let path = fixture.home.join(relative_path);
    if let Some(bytes) = bytes {
        fs::write(&path, vec![0; bytes]).unwrap();
    }
    path
}

fn assert_deleted(fixture: &Fixture, id: Uuid, path: &std::path::Path) {
    assert!(!path.exists());
    block_on(async {
        let state = State::open_existing(&fixture.home).await.unwrap();
        assert!(state.by_id(id).await.unwrap().is_none());
    });
}

#[test]
fn new_keeps_archives_when_no_limits_are_configured() {
    for limits in [
        Value::Null,
        json!({}),
        json!({"after_count": null, "after_hours": null, "after_kbytes": null}),
    ] {
        let fixture = Fixture::previous_trial();
        configure(&fixture, limits);
        let id = fixture.active().id;
        fixture.run(new(&[]), |_| Ok(())).unwrap();
        assert!(
            fixture
                .home
                .join(fixture.record(id).archived_path.unwrap())
                .is_file()
        );
    }
}

#[test]
fn zero_count_removes_the_just_archived_project_and_missing_archive_records() {
    let fixture = Fixture::previous_trial();
    let current = fixture.active().id;
    let missing = Uuid::new_v4();
    let missing_path = seed_archive(&fixture, missing, "2000-01-01T00:00:00Z", None);
    let unregistered = fixture.home.join("history/unregistered.bundle");
    fs::write(&unregistered, "keep").unwrap();
    configure(&fixture, json!({"after_count": 0}));

    fixture.run(new(&[]), |_| Ok(())).unwrap();

    assert_deleted(
        &fixture,
        current,
        &fixture.home.join(format!("history/{current}.bundle")),
    );
    assert_deleted(&fixture, missing, &missing_path);
    assert!(unregistered.is_file());
    assert_ne!(fixture.active().id, current);
}

#[test]
fn count_limit_removes_the_tail_in_reverse_list_order() {
    let fixture = Fixture::empty();
    fixture.run(Action::Init, not_opened).unwrap();
    let first = Uuid::from_u128(1);
    let last = Uuid::from_u128(2);
    let time = "2000-01-01T00:00:00.1Z";
    let kept = seed_archive(&fixture, first, time, Some(1));
    let tied = seed_archive(&fixture, last, time, Some(1));
    let earlier = Uuid::from_u128(3);
    let earlier_bundle = seed_archive(&fixture, earlier, "2000-01-01T00:00:00Z", Some(1));
    configure(&fixture, json!({"after_count": 1}));

    fixture.run(new(&[]), |_| Ok(())).unwrap();

    assert!(kept.is_file());
    fixture.record(first);
    assert_deleted(&fixture, last, &tied);
    assert_deleted(&fixture, earlier, &earlier_bundle);
}

#[test]
fn fractional_hours_expire_archives_by_last_visit() {
    let fixture = Fixture::empty();
    fixture.run(Action::Init, not_opened).unwrap();
    let now = Timestamp::now().as_second();
    let oldest = Uuid::from_u128(1);
    let recent = Uuid::from_u128(2);
    let expired = Timestamp::from_second(now - 100 * 60).unwrap().to_string();
    let retained = Timestamp::from_second(now - 80 * 60).unwrap().to_string();
    let expired_bundle = seed_archive(&fixture, oldest, &expired, Some(1));
    let retained_bundle = seed_archive(&fixture, recent, &retained, Some(1));
    configure(&fixture, json!({"after_hours": 1.5}));

    fixture.run(new(&[]), |_| Ok(())).unwrap();

    assert_deleted(&fixture, oldest, &expired_bundle);
    assert!(retained_bundle.is_file());
    fixture.record(recent);
}

#[test]
fn exceeding_only_the_size_limit_removes_oldest_until_the_exact_limit() {
    let fixture = Fixture::empty();
    fixture.run(Action::Init, not_opened).unwrap();
    let oldest = Uuid::from_u128(1);
    let recent = Uuid::from_u128(2);
    let removed = seed_archive(&fixture, oldest, "2000-01-01T00:00:00Z", Some(1));
    let kept = seed_archive(&fixture, recent, "2000-01-02T00:00:00Z", Some(1024));
    configure(
        &fixture,
        json!({"after_count": 5, "after_hours": 1e10, "after_kbytes": 1}),
    );

    fixture.run(new(&[]), |_| Ok(())).unwrap();

    assert_deleted(&fixture, oldest, &removed);
    assert!(kept.is_file());
    fixture.record(recent);
}

#[test]
fn invalid_hour_limit_is_rejected_before_clear() {
    let fixture = Fixture::previous_trial();
    let old = fixture.active();
    configure(&fixture, json!({"after_hours": -0.5}));

    let error = fixture.run(new(&[]), not_opened).unwrap_err();

    assert!(format!("{error:#}").contains("auto_delete.after_hours"));
    assert_eq!(fixture.active(), old);
    assert!(fixture.play.join("old.txt").is_file());
    fixture.assert_no_archives();
}

#[test]
fn auto_delete_rejects_an_archive_path_that_is_not_a_file() {
    let fixture = Fixture::previous_trial();
    let current = fixture.active().id;
    let id = Uuid::from_u128(1);
    let path = seed_archive(&fixture, id, "2000-01-01T00:00:00Z", None);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), "contents").unwrap();
    configure(&fixture, json!({"after_count": 0}));

    let error = fixture.run(new(&[]), not_opened).unwrap_err();

    assert!(format!("{error:#}").contains("必须是普通文件"));
    assert!(path.join("keep").is_file());
    fixture.record(id);
    assert!(fixture.record(current).archived_path.is_some());
    assert!(fs::read_dir(&fixture.play).unwrap().next().is_none());
}

#[test]
fn clear_does_not_trigger_auto_delete() {
    let fixture = Fixture::previous_trial();
    let id = fixture.active().id;
    configure(&fixture, json!({"after_count": 0}));

    fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap();

    assert!(
        fixture
            .home
            .join(fixture.record(id).archived_path.unwrap())
            .is_file()
    );
}

#[test]
fn newly_archived_project_starts_at_zero_hours_but_still_counts_towards_size() {
    for after_kbytes in [None, Some(0)] {
        let fixture = Fixture::previous_trial();
        let id = fixture.active().id;
        let old_time = "2000-01-01T00:00:00Z";
        block_on(async {
            let state = State::open_existing(&fixture.home).await.unwrap();
            state
                .connection()
                .execute(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE playgrounds SET last_visit_time = ? WHERE id = ?",
                    [old_time.into(), id.into()],
                ))
                .await
                .unwrap();
        });
        let stale_id = Uuid::from_u128(1);
        let stale_bundle = seed_archive(&fixture, stale_id, old_time, Some(1));
        configure(
            &fixture,
            json!({"after_hours": 0, "after_kbytes": after_kbytes}),
        );
        let before_new = Timestamp::now();

        fixture.run(new(&[]), |_| Ok(())).unwrap();

        assert_deleted(&fixture, stale_id, &stale_bundle);
        let path = fixture.home.join(format!("history/{id}.bundle"));
        if after_kbytes.is_some() {
            assert_deleted(&fixture, id, &path);
        } else {
            assert!(path.is_file());
            let refreshed = fixture
                .record(id)
                .last_visit_time
                .parse::<Timestamp>()
                .unwrap();
            assert!((before_new..=Timestamp::now()).contains(&refreshed));
        }
    }
}
