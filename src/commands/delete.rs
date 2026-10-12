use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use clap::Args;

use crate::{
    app::Cli,
    arg_types::IdentifierToken,
    commands::Command,
    common::{DIM, GREEN, ICONS, colored},
    wire::{
        task::TaskPatch,
        wire_object::{EntityType, WireObject},
    },
};

#[derive(Debug, Args)]
#[command(about = "Delete tasks/projects/headings/areas")]
pub struct DeleteArgs {
    /// Item UUID(s) (or unique UUID prefixes)
    pub item_ids: Vec<IdentifierToken>,
}

#[derive(Debug, Clone)]
struct DeletePlan {
    targets: Vec<(String, EntityType, String)>,
    changes: BTreeMap<String, WireObject>,
}

fn build_delete_plan(
    args: &DeleteArgs,
    store: &crate::store::ThingsStore,
    now: f64,
) -> Result<DeletePlan, String> {
    let mut targets: Vec<(String, EntityType, String)> = Vec::new();
    let mut seen = HashSet::new();

    for identifier in &args.item_ids {
        let (task, task_err, task_ambiguous) = store.resolve_task_identifier(identifier.as_str());
        let (area, _area_err, area_ambiguous) = store.resolve_area_identifier(identifier.as_str());
        let task_hits = usize::from(task.is_some()) + task_ambiguous.len();
        let area_hits = usize::from(area.is_some()) + area_ambiguous.len();
        let raw = identifier.as_str();

        // One namespace must have one hit and the other none. An ambiguous
        // prefix in either namespace used to fall through to the unique side.
        if !matches!((task_hits, area_hits), (1, 0) | (0, 1)) {
            if task_hits == 0 && area_hits == 0 {
                return Err(format!("Item not found: {raw}"));
            }
            let _ = task_err;
            return Err(format!(
                "Ambiguous identifier '{raw}' ({task_hits} tasks, {area_hits} areas). Refusing to delete."
            ));
        }

        if let Some(task) = task {
            if !task.entity.can_upgrade_to_task7() {
                return Err(format!(
                    "Unsupported task entity for deletion: {}",
                    task.entity
                ));
            }
            if task.trashed {
                return Err(format!("Item already in Trash: {}", task.title));
            }
            if task.has_repeater() || task.is_recurrence_template() {
                return Err(format!(
                    "Recurring tasks are blocked from deletion until recurrence bookkeeping is supported: {}",
                    task.title
                ));
            }
            if !seen.insert(task.uuid.clone()) {
                continue;
            }
            targets.push((task.uuid.to_string(), EntityType::Task7, task.title.clone()));
            continue;
        }

        if let Some(area) = area {
            if !seen.insert(area.uuid.clone()) {
                continue;
            }
            targets.push((area.uuid.to_string(), EntityType::Area3, area.title.clone()));
        }
    }

    let mut changes = BTreeMap::new();
    for (uuid, entity, _title) in &targets {
        // Tasks go to Trash (`tr`) so Things can restore them. `t: 2` removes
        // the history object. Areas have no trash flag in this client.
        let change = if entity == &EntityType::Task7 {
            WireObject::update(
                EntityType::Task7,
                TaskPatch {
                    trashed: Some(true),
                    modification_date: Some(Some(now)),
                    ..Default::default()
                },
            )
        } else {
            WireObject::delete(entity.clone())
        };
        changes.insert(uuid.clone(), change);
    }

    Ok(DeletePlan { targets, changes })
}

impl Command for DeleteArgs {
    fn run_with_ctx(
        &self,
        cli: &Cli,
        out: &mut dyn std::io::Write,
        ctx: &mut dyn crate::cmd_ctx::CmdCtx,
    ) -> Result<()> {
        let store = cli.load_store()?;
        let plan = build_delete_plan(self, &store, ctx.now_timestamp())
            .map_err(|err| anyhow::anyhow!(err))?;

        if plan.targets.is_empty() {
            return Ok(());
        }

        ctx.commit_changes(plan.changes, None)
            .map_err(|e| anyhow::anyhow!("Failed to delete items: {e}"))?;

        for (uuid, _entity, title) in plan.targets {
            writeln!(
                out,
                "{} {}  {}",
                colored(format!("{} Deleted", ICONS.deleted), &[GREEN], cli.no_color),
                title,
                colored(&uuid, &[DIM], cli.no_color)
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{
        store::{ThingsStore, fold_items},
        wire::{
            area::AreaProps,
            task::{TaskProps, TaskStart, TaskStatus, TaskType},
        },
    };

    const TASK_A: &str = "A7h5eCi24RvAWKC3Hv3muf";
    const TASK_B: &str = "KGvAPpMrzHAKMdgMiERP1V";
    const AREA_A: &str = "MpkEei6ybkFS2n6SXvwfLf";

    struct FailingCtx;

    impl crate::cmd_ctx::CmdCtx for FailingCtx {
        fn now_timestamp(&self) -> f64 {
            1.0
        }

        fn today_timestamp(&self) -> i64 {
            unreachable!()
        }

        fn next_id(&mut self) -> String {
            unreachable!()
        }

        fn current_head_index(&self) -> i64 {
            unreachable!()
        }

        fn commit_changes(
            &mut self,
            _changes: BTreeMap<String, WireObject>,
            _ancestor_index: Option<i64>,
        ) -> Result<i64> {
            anyhow::bail!("cloud unavailable")
        }
    }

    #[test]
    fn delete_propagates_commit_failure_without_success_output() {
        let journal = tempfile::NamedTempFile::new().expect("journal file");
        let item = BTreeMap::from([task(TASK_A, "Alpha", false)]);
        serde_json::to_writer(&journal, &vec![item]).expect("write journal");
        let cli = Cli::parse_from([
            "things3",
            "--load-journal",
            journal.path().to_str().expect("journal path"),
        ]);
        let args = DeleteArgs {
            item_ids: vec![IdentifierToken::from(TASK_A)],
        };
        let mut out = Vec::new();

        let error = args
            .run_with_ctx(&cli, &mut out, &mut FailingCtx)
            .expect_err("failed commit must fail the command");

        assert_eq!(
            error.to_string(),
            "Failed to delete items: cloud unavailable"
        );
        assert!(out.is_empty());
    }

    fn build_store(entries: Vec<(String, WireObject)>) -> ThingsStore {
        let mut item = BTreeMap::new();
        for (uuid, obj) in entries {
            item.insert(uuid, obj);
        }
        ThingsStore::from_raw_state(&fold_items([item]))
    }

    fn task(uuid: &str, title: &str, trashed: bool) -> (String, WireObject) {
        (
            uuid.to_string(),
            WireObject::create(
                EntityType::Task6,
                TaskProps {
                    title: title.to_string(),
                    item_type: TaskType::Todo,
                    status: TaskStatus::Incomplete,
                    start_location: TaskStart::Inbox,
                    sort_index: 0,
                    trashed,
                    creation_date: Some(1.0),
                    modification_date: Some(1.0),
                    ..Default::default()
                },
            ),
        )
    }

    fn area(uuid: &str, title: &str) -> (String, WireObject) {
        (
            uuid.to_string(),
            WireObject::create(
                EntityType::Area3,
                AreaProps {
                    title: title.to_string(),
                    sort_index: 0,
                    ..Default::default()
                },
            ),
        )
    }

    #[test]
    fn delete_payloads_match_snapshot_cases() {
        let single = build_delete_plan(
            &DeleteArgs {
                item_ids: vec![IdentifierToken::from(TASK_A)],
            },
            &build_store(vec![task(TASK_A, "Alpha", false)]),
            1.0,
        )
        .expect("plan");
        assert_eq!(
            serde_json::to_value(single.changes).expect("to value"),
            serde_json::json!({ TASK_A: {"t":1,"e":"Task7","p":{"md":1.0,"tr":true}} })
        );

        let multi = build_delete_plan(
            &DeleteArgs {
                item_ids: vec![IdentifierToken::from(TASK_A), IdentifierToken::from(AREA_A)],
            },
            &build_store(vec![task(TASK_A, "Alpha", false), area(AREA_A, "Work")]),
            1.0,
        )
        .expect("plan");
        assert_eq!(
            serde_json::to_value(multi.changes).expect("to value"),
            serde_json::json!({
                TASK_A: {"t":1,"e":"Task7","p":{"md":1.0,"tr":true}},
                AREA_A: {"t":2,"e":"Area3","p":{}}
            })
        );

        let already_trashed = build_delete_plan(
            &DeleteArgs {
                item_ids: vec![IdentifierToken::from(TASK_A), IdentifierToken::from(TASK_B)],
            },
            &build_store(vec![
                task(TASK_A, "Active", false),
                task(TASK_B, "Trashed", true),
            ]),
            1.0,
        );
        assert!(already_trashed.is_err());
    }
}
