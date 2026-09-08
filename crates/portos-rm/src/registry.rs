//! Runtime admission is closed to the reviewed algebras. The mathematical Ra
//! trait remains open; these wrappers attest validity, not algebraic proofs.
use crate::identity::{ClassId, PoolId};
use crate::ledger::{AlgebraTag, Frag, LedgerError};
use crate::ra::{Count, Ex, Frac, GSet, Ra, Ranges};
use std::marker::PhantomData;

mod sealed {
    pub trait Sealed {}
}
pub trait RuntimeAlgebra: Ra + sealed::Sealed {
    const TAG: AlgebraTag;
    fn into_fragment(self) -> Frag;
    fn from_fragment(value: &Frag) -> Option<&Self>;
}
macro_rules! algebra {
    ($ty:ty, $variant:ident, $tag:ident) => {
        impl sealed::Sealed for $ty {}
        impl RuntimeAlgebra for $ty {
            const TAG: AlgebraTag = AlgebraTag::$tag;
            fn into_fragment(self) -> Frag {
                Frag::$variant(self)
            }
            fn from_fragment(value: &Frag) -> Option<&Self> {
                match value {
                    Frag::$variant(v) => Some(v),
                    _ => None,
                }
            }
        }
    };
}
algebra!(Ex, Ex, Exclusive);
algebra!(Count, Count, Counted);
algebra!(GSet, Set, Set);
algebra!(Ranges, Range, Range);
algebra!(Frac, Frac, Frac);

macro_rules! valid_value {
    ($name:ident) => {
        #[derive(Clone, Debug)]
        pub struct $name<A: RuntimeAlgebra>(A);
        impl<A: RuntimeAlgebra> $name<A> {
            pub fn new(value: A) -> Result<Self, LedgerError> {
                if !value.valid() {
                    return Err(LedgerError::InvalidValue);
                }
                Ok(Self(value))
            }
            pub fn value(&self) -> &A {
                &self.0
            }
            pub(crate) fn into_fragment(self) -> Frag {
                self.0.into_fragment()
            }
        }
    };
}
valid_value!(Capacity);
valid_value!(Claim);

#[derive(Clone, Debug)]
pub struct ClassBinding {
    pub(crate) ledger: u64,
    pub(crate) id: ClassId,
    pub(crate) algebra: AlgebraTag,
}
impl ClassBinding {
    pub fn for_algebra<A: RuntimeAlgebra>(&self) -> Result<RegisteredClass<A>, LedgerError> {
        if self.algebra != A::TAG {
            return Err(LedgerError::AlgebraMismatch);
        }
        Ok(RegisteredClass {
            binding: self.clone(),
            marker: PhantomData,
        })
    }
    pub fn id(&self) -> &ClassId {
        &self.id
    }
}

#[derive(Clone, Debug)]
pub struct RegisteredClass<A: RuntimeAlgebra> {
    pub(crate) binding: ClassBinding,
    marker: PhantomData<A>,
}
impl<A: RuntimeAlgebra> RegisteredClass<A> {
    pub fn id(&self) -> &ClassId {
        self.binding.id()
    }
}

/// Scoped to the ledger view that issued it. Re-query after cloning or reopening.
#[derive(Clone, Debug)]
pub struct PoolRef<A: RuntimeAlgebra> {
    pub(crate) ledger: u64,
    pub(crate) id: PoolId,
    marker: PhantomData<A>,
}
impl<A: RuntimeAlgebra> PoolRef<A> {
    pub(crate) fn new(ledger: u64, id: PoolId) -> Self {
        Self {
            ledger,
            id,
            marker: PhantomData,
        }
    }
    pub fn id(&self) -> &PoolId {
        &self.id
    }
}

/// A counted pool cannot accept a fractional request.
/// ```compile_fail
/// use portos_rm::{ledger::{Ledger, GrantRequest}, registry::PoolRef, ra::{Count, Frac}};
/// fn mixed(l: &mut Ledger, pool: &PoolRef<Count>, request: GrantRequest<Frac>) {
///     l.grant(pool, request);
/// }
/// ```
/// Pool and registry bindings have no public constructor.
/// ```compile_fail
/// use portos_rm::{registry::PoolRef, ra::Count};
/// let pool = PoolRef::<Count> { ledger: 1, id: todo!(), marker: Default::default() };
/// ```
/// ```compile_fail
/// use portos_rm::{registry::RegisteredClass, ra::Count};
/// let class = RegisteredClass::<Count> { binding: todo!(), marker: Default::default() };
/// ```
const _: () = ();
