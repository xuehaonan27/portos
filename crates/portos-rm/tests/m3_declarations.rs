use portos_rm::identity::{ClassId, VerbId};
use portos_rm::ledger::RevertGrade;
use portos_rm::protocol::{ProtocolDraft, ProtocolError};
use portos_rm::verbs::*;

fn draft() -> ClassDeclarationDraft {
    let mut d = ClassDeclarationDraft::new(ClassId::new("device"));
    d.holding_grade = Some(RevertGrade::Inverse);
    d.verbs.push((VerbId::new("read"), VerbEntry::repeatable()));
    d
}

#[test]
fn all_cross_references_are_checked_before_admission() {
    let mut bad = draft();
    bad.verbs.push((
        VerbId::new("write"),
        VerbEntry::transforming().degrades_to("missing"),
    ));
    assert!(matches!(bad.check(), Err(VerbError::Incoherent(_))));

    let mut bad = draft();
    bad.verbs.push((
        VerbId::new("send"),
        VerbEntry::emitting(
            EmitGrade::Compensable {
                compensate_with: VerbId::new("missing"),
            },
            true,
        ),
    ));
    assert!(matches!(bad.check(), Err(VerbError::Incoherent(_))));

    let mut bad = draft();
    bad.protocol = Some(
        ProtocolDraft::new("start")
            .transition("start", "missing", "end")
            .check()
            .unwrap(),
    );
    assert!(matches!(bad.check(), Err(VerbError::Incoherent(_))));

    let mut bad = draft();
    bad.holding_grade = None;
    bad.verbs.push((
        VerbId::new("acquire"),
        VerbEntry::consuming(ConsumeGrade::Held),
    ));
    assert_eq!(bad.check(), Err(VerbError::ClassNotDeclared));
}

#[test]
fn duplicate_verbs_and_class_replacement_cannot_change_a_published_meaning() {
    let mut duplicate = draft();
    duplicate.verbs.push((
        VerbId::new("read"),
        VerbEntry::emitting(EmitGrade::External, false),
    ));
    assert_eq!(duplicate.check(), Err(VerbError::DuplicateVerb));
    let mut table = VerbTable::new();
    table.insert(draft().check().unwrap()).unwrap();
    let mut replacement = draft();
    replacement.verbs[0].1 = VerbEntry::emitting(EmitGrade::External, false);
    assert_eq!(
        table.insert(replacement.check().unwrap()),
        Err(VerbError::ClassAlreadyDeclared)
    );
    assert_eq!(
        table
            .lookup(&ClassId::new("device"), &VerbId::new("read"))
            .unwrap()
            .kind(),
        &Kind::Repeatable
    );
    assert_eq!(
        table.derive_handler_policy(&ClassId::new("missing")),
        Err(VerbError::Unknown)
    );
}

#[test]
fn raw_flags_remain_unscoped_provider_assertions() {
    let class = draft().check().unwrap();
    assert_eq!(class.laws().len(), 2);
    assert!(class.laws().iter().all(|l| l.scope == LawScope::Unspecified
        && matches!(l.source, EvidenceSource::ProviderAssertion(_))));
    assert!(class.laws().iter().any(|l| matches!(&l.equation, Equation::LegacyCommutationSummary { operation } if operation.as_str() == "read")));
}

#[test]
fn new_equations_name_operations_parameters_observations_and_assumptions() {
    let mut d = draft();
    d.laws.push(LawDeclaration {
        equation: Equation::Commutes {
            left: VerbId::new("read"),
            right: VerbId::new("read"),
        },
        scope: LawScope::Declared {
            parameters: "same immutable snapshot".into(),
            observations: "returned values and snapshot bytes".into(),
            assumptions: "no clock, counters or external side effects".into(),
        },
        source: EvidenceSource::ReviewedContract("fixture snapshot read contract".into()),
    });
    assert!(d.clone().check().is_ok());
    let mut unknown = d.clone();
    unknown.laws[0].equation = Equation::Commutes {
        left: VerbId::new("read"),
        right: VerbId::new("write"),
    };
    assert!(matches!(unknown.check(), Err(VerbError::Incoherent(_))));
    let mut unscoped = d.clone();
    unscoped.laws[0].scope = LawScope::Unspecified;
    assert!(matches!(unscoped.check(), Err(VerbError::Incoherent(_))));
    if let LawScope::Declared { observations, .. } = &mut d.laws[0].scope {
        observations.clear();
    }
    assert!(matches!(d.check(), Err(VerbError::Incoherent(_))));
}

#[test]
fn protocol_duplicate_transitions_are_preserved_until_rejection() {
    for target in ["open", "closed"] {
        let duplicate = ProtocolDraft::new("closed")
            .transition("closed", "open", "open")
            .transition("closed", "open", target);
        assert_eq!(duplicate.check(), Err(ProtocolError::DuplicateTransition));
    }
    assert_eq!(
        ProtocolDraft::new("").check(),
        Err(ProtocolError::EmptyName)
    );
}
