//! Compact fixture syntax; finalization uses the production class checker.
use crate::identity::{ClassId, VerbId};
use crate::ledger::RevertGrade;
use crate::protocol::Protocol;
use crate::verbs::{ClassDeclarationDraft, VerbEntry, VerbError, VerbTable};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Declarations(BTreeMap<ClassId, ClassDeclarationDraft>);
impl Declarations {
    pub fn new() -> Self {
        Self::default()
    }
    fn class(&mut self, name: &str) -> &mut ClassDeclarationDraft {
        let id = ClassId::new(name);
        self.0
            .entry(id.clone())
            .or_insert_with(|| ClassDeclarationDraft::new(id))
    }
    pub fn declare_class(&mut self, name: &str, grade: RevertGrade) -> Result<(), VerbError> {
        let c = self.class(name);
        if c.holding_grade.is_some() {
            return Err(VerbError::ClassAlreadyDeclared);
        }
        c.holding_grade = Some(grade);
        Ok(())
    }
    pub fn register(&mut self, class: &str, verb: &str, entry: VerbEntry) -> Result<(), VerbError> {
        self.class(class).verbs.push((VerbId::new(verb), entry));
        Ok(())
    }
    pub fn declare_protocol(&mut self, class: &str, protocol: Protocol) -> Result<(), VerbError> {
        let c = self.class(class);
        if c.protocol.is_some() {
            return Err(VerbError::ClassAlreadyDeclared);
        }
        c.protocol = Some(protocol);
        Ok(())
    }
    pub fn check_all(&self) -> Result<VerbTable, VerbError> {
        let mut table = VerbTable::new();
        for draft in self.0.values() {
            table.insert(draft.clone().check()?)?;
        }
        Ok(table)
    }
}
