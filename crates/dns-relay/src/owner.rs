//! The identity a relayed name denotes.

use std::fmt::Debug;

/// The identity a relayed name denotes. In Rings it is a DID; the relay keeps it abstract, so it
/// names no overlay type, and the node implements it for its DID.
///
/// The label is the owner's canonical text, and the relay parses only canonical labels, so an
/// owner has exactly one name in each zone.
///
/// ```text
///   label      : O → Label        lowercase letters, digits and hyphens, at most 63 bytes
///   from_label : Label ⇀ O
///   Law (round trip):  from_label(label(o)) = Some(o)
///   Law (canonical):   from_label(l) = Some(o)  ⇒  label(o) = l
/// ```
///
/// Together the laws make `label` a bijection between owners and the labels that parse, which
/// the address map relies on: it hashes the label, so distinct owners hash distinct inputs.
pub trait Owner: Copy + Ord + Debug + Send + Sync + 'static {
    /// The owner the lowercase label `label` names, or `None` when it names none.
    fn from_label(label: &str) -> Option<Self>;

    /// The canonical label naming this owner.
    fn label(&self) -> String;
}

#[cfg(test)]
pub(crate) mod test_owner {
    //! A small owner for the crate's tests: `n` is named `o<n>` in decimal.

    use super::Owner;

    /// An owner named `o<n>`.
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(crate) struct TestOwner(pub(crate) u32);

    impl Owner for TestOwner {
        fn from_label(label: &str) -> Option<Self> {
            let digits = label.strip_prefix('o')?;
            let n = digits.parse::<u32>().ok()?;
            let owner = Self(n);
            (owner.label() == label).then_some(owner)
        }

        fn label(&self) -> String {
            format!("o{}", self.0)
        }
    }

    #[test]
    fn test_test_owner_satisfies_the_label_laws() {
        for n in [0, 1, 42, u32::MAX] {
            let owner = TestOwner(n);
            assert_eq!(TestOwner::from_label(&owner.label()), Some(owner));
        }
        for label in ["o01", "o", "x1", "o-1", "O1"] {
            assert_eq!(TestOwner::from_label(label), None, "{label}");
        }
    }
}
