use super::*;
use portos_rm::cleanup::CleanupPolicy;
use portos_rm::identity::{ClassId, Generation, InstanceId};
use portos_rm::ledger::{AlgebraTag, ClassDecl, GrantRequest, RevertGrade};
use portos_rm::ra::Ex;
use portos_rm::registry::{Capacity, Claim};
use portos_rm::time::LeaseRequest;

#[test]
fn recovery_uses_the_persisted_segment_association() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../.dev/tmp/root-m3-scope-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = crate::host::Host::new(kernel.clone(), &root.join("sock")).unwrap();
    let run = host.plans.submit("user", br#"{"stmts":[]}"#).unwrap();
    let old_label = SubjectId::new(format!("plan:{}#{}:seg", run.plan_hash, run.run_id));
    let segment = SubjectId::new("explicit:segment/with#delimiters");
    let holdings = kernel
        .ledger
        .transaction(|tx| {
            let class = ClassId::new("scope-fixture");
            tx.register_class(ClassDecl {
                class_id: class.clone(),
                algebra: AlgebraTag::Exclusive,
                cleanup: CleanupPolicy::AccountingOnly,
                release_idempotent: true,
                lease_duration: None,
                revert_grade: RevertGrade::Inverse,
            })?;
            let class = tx.registered_class::<Ex>(&class)?;
            let mut holdings = Vec::new();
            for (i, subject) in [&segment, &old_label].into_iter().enumerate() {
                let pool = tx.create_pool(
                    &class,
                    InstanceId::new(i.to_string()),
                    Capacity::new(Ex::Token).unwrap(),
                )?;
                holdings.push(tx.grant(
                    &pool,
                    GrantRequest {
                        owner: subject.clone(),
                        claim: Claim::new(Ex::Token).unwrap(),
                        generation: Generation::new("g"),
                        parent: None,
                        lease: LeaseRequest::Unbounded,
                        now: Timestamp::ZERO,
                    },
                )?);
            }
            Ok(holdings)
        })
        .unwrap();
    {
        let conn = kernel.db.lock().unwrap();
        conn.execute(
            "UPDATE plan_segments SET subject=?2 WHERE run_id=?1",
            params![run.run_id, segment.as_str()],
        )
        .unwrap();
        conn.execute(
            "UPDATE plan_runs SET state='running' WHERE run_id=?1",
            params![run.run_id],
        )
        .unwrap();
    }
    host.plans.recover();
    assert!(
        kernel
            .ledger
            .holding(holdings[0].id())
            .unwrap()
            .unwrap()
            .released_at()
            .is_some()
    );
    assert!(
        kernel
            .ledger
            .holding(holdings[1].id())
            .unwrap()
            .unwrap()
            .state
            .is_active()
    );
    drop(host);
    drop(kernel);
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = crate::host::Host::new(kernel.clone(), &root.join("sock")).unwrap();
    assert_eq!(host.plans.segment_of(&run.run_id).unwrap(), segment);
    assert!(
        kernel
            .ledger
            .holding(holdings[1].id())
            .unwrap()
            .unwrap()
            .state
            .is_active()
    );
    drop(host);
    drop(kernel);
    std::fs::remove_dir_all(root).unwrap();
}
