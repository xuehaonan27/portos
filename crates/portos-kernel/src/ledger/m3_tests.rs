use super::*;

#[test]
fn account_names_cannot_alias_pool_identity_across_restart_and_settlement() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../.dev/tmp/root-m3-accounts-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
    let (ledger, _) = LedgerStore::open(db.clone()).unwrap();
    let pairs = [
        (AccountId::new("a/b"), EffectClass::new("c")),
        (AccountId::new("a"), EffectClass::new("b/c")),
    ];
    let owner = SubjectId::new("owner:seg:spent");
    ledger
        .transaction(|tx| {
            let mut keys = Vec::new();
            for (account, effect) in &pairs {
                let pool = tx.create_count_pool(
                    account,
                    effect,
                    &owner,
                    Capacity::new(Count::Value(5)).unwrap(),
                )?;
                keys.push(pool.id().clone());
            }
            assert_ne!(keys[0], keys[1]);
            tx.spend(
                &owner,
                &SpendRequest {
                    account: pairs[0].0.clone(),
                    effect: pairs[0].1.clone(),
                    amount: 3,
                },
                Timestamp::ZERO,
            )?;
            Ok(())
        })
        .unwrap();
    drop(ledger);
    drop(db);
    let db = Arc::new(Mutex::new(crate::db::open(&root).unwrap()));
    let (ledger, _) = LedgerStore::open(db.clone()).unwrap();
    assert_eq!(
        ledger.count_balance(&pairs[0].0, &pairs[0].1).unwrap(),
        Some(2)
    );
    assert_eq!(
        ledger.count_balance(&pairs[1].0, &pairs[1].1).unwrap(),
        Some(5)
    );
    ledger
        .transaction(|tx| {
            let existing = tx.count_pool(&pairs[0].0, &pairs[0].1)?;
            let repeated = tx.create_count_pool(
                &pairs[0].0,
                &pairs[0].1,
                &owner,
                Capacity::new(Count::Value(5)).unwrap(),
            )?;
            assert_eq!(existing.id(), repeated.id());
            tx.settle_and_zero_pool(&existing, Timestamp::ZERO)
        })
        .unwrap();
    assert_eq!(
        ledger.count_balance(&pairs[0].0, &pairs[0].1).unwrap(),
        Some(0)
    );
    assert_eq!(
        ledger.count_balance(&pairs[1].0, &pairs[1].1).unwrap(),
        Some(5)
    );
    drop(ledger);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
