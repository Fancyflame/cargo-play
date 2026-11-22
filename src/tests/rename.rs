use super::*;

fn rename(fixture: &Fixture, args: &[&str]) -> anyhow::Result<()> {
    let action =
        Cli::try_parse_from(["cgpl", "rename"].into_iter().chain(args.iter().copied()))?.command;
    fixture.run(action, not_opened)
}

#[test]
fn rejects_invalid_rename_arguments() {
    for args in [
        &[][..],
        &["2"],
        &["1", ""],
        &["0", "新名称"],
        &["-1", "新名称"],
        &["null", "新名称"],
        &["1", "新名称", "多余参数"],
    ] {
        assert!(
            Cli::try_parse_from(["cgpl", "rename"].into_iter().chain(args.iter().copied()))
                .is_err(),
            "accepted invalid arguments: {args:?}"
        );
    }
}

#[test]
fn renames_current_and_archived_playgrounds_without_changing_list_order() {
    let fixture = Fixture::previous_trial();
    let archived_id = fixture.active().id;
    fixture.run(new(&[]), |_| Ok(())).unwrap();
    let archived = fixture.record(archived_id);
    let current = fixture.active();
    let manifest = fs::read(fixture.play.join("Cargo.toml")).unwrap();
    let bundle = fixture.home.join(archived.archived_path.as_ref().unwrap());
    let bundle_contents = fs::read(&bundle).unwrap();

    rename(&fixture, &["1", "新名称 含空格"]).unwrap();
    rename(&fixture, &["2", "2"]).unwrap();
    assert_eq!(
        fixture.active(),
        PlaygroundRecord {
            name: "新名称 含空格".into(),
            ..current
        }
    );
    assert_eq!(
        fixture.record(archived_id),
        PlaygroundRecord {
            name: "2".into(),
            ..archived.clone()
        }
    );
    let entries = block_on(State::list(&fixture.home, 0)).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.id, entry.name.as_str()))
            .collect::<Vec<_>>(),
        [(current.id, "新名称 含空格"), (archived_id, "2")]
    );
    assert_eq!(fs::read(fixture.play.join("Cargo.toml")).unwrap(), manifest);
    assert_eq!(fs::read(bundle).unwrap(), bundle_contents);

    let missing_position = rename(&fixture, &["3", "未使用"]).unwrap_err();
    assert!(missing_position.to_string().contains("找不到序号为 3"));
    let missing_id = rename(&fixture, &[&Uuid::nil().to_string(), "未使用"]).unwrap_err();
    assert!(missing_id.to_string().contains("找不到演练场"));

    fixture
        .run(Action::Clear { force: false }, not_opened)
        .unwrap();
    assert_eq!(fixture.record(archived_id).name, "2");
    rename(&fixture, &[&archived_id.to_string(), "归档新名称"]).unwrap();
    assert_eq!(
        fixture.record(archived_id),
        PlaygroundRecord {
            name: "归档新名称".into(),
            ..archived
        }
    );
    rename(&fixture, &["1", "清空后的首项"]).unwrap();
    assert_eq!(fixture.record(current.id).name, "清空后的首项");
}
