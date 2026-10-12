//! Field metadata flags: where each field goes in a transaction's metadata (independence task 2.1).
//!
//! rippled gives every serialized field a set of `kSmd*` flags (`SField.h`). When a transaction
//! applies, `ApplyStateTable::apply` (src/libxrpl/ledger/ApplyStateTable.cpp:160-245 in 3.4.1) uses
//! them to build each AffectedNodes entry:
//! - a ModifiedNode's or DeletedNode's PreviousFields hold the original value of every
//!   `kSmdChangeOrig` field that changed;
//! - a ModifiedNode's FinalFields hold every `kSmdAlways` or `kSmdChangeNew` field of the new object;
//! - a DeletedNode's FinalFields hold every `kSmdAlways` or `kSmdDeleteFinal` field of the last one;
//! - a CreatedNode's NewFields hold every `kSmdCreate` or `kSmdAlways` field whose value is not
//!   the default.
//!
//! `table.rs` is generated from rippled's source by `scripts/gen_field_meta.py`. This file holds the
//! lookups. Step 2's record builder takes its flags from here; the type code is kept for encoding.

mod table;

pub use table::{
    SMD_ALWAYS, SMD_BASE_TEN, SMD_CHANGE_NEW, SMD_CHANGE_ORIG, SMD_CREATE, SMD_DEFAULT, SMD_DELETE_FINAL,
    SMD_NEEDS_ASSET, SMD_NEVER, SMD_PSEUDO_ACCOUNT,
};

/// One rippled field (`SField`) and its metadata flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldMeta {
    /// rippled's field code, `(type_code << 16) | nth` (`SField::fieldCode`).
    pub code: u32,
    /// The serialized type (`STI_*`): 1 UINT16, 2 UINT32, 6 AMOUNT, 8 ACCOUNT, 14 OBJECT, 15 ARRAY, …
    pub type_code: u16,
    /// The field's number within its type (`SField::fieldValue`).
    pub nth: u16,
    /// The name without rippled's `sf` prefix, as the JSON shows it ("Balance").
    pub name: &'static str,
    /// The `SMD_*` bits.
    pub meta: u8,
}

/// The four places a field can appear inside an AffectedNodes entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    /// A ModifiedNode's or DeletedNode's PreviousFields (only when the value changed).
    PreviousFields,
    /// A ModifiedNode's FinalFields.
    ModifiedFinalFields,
    /// A DeletedNode's FinalFields.
    DeletedFinalFields,
    /// A CreatedNode's NewFields (only a value that is not the default).
    NewFields,
}

impl Section {
    /// Every section, in the order above.
    pub const ALL: [Section; 4] =
        [Section::PreviousFields, Section::ModifiedFinalFields, Section::DeletedFinalFields, Section::NewFields];

    /// The flags that let a field appear here; any one is enough (`SField::shouldMeta`).
    pub const fn mask(self) -> u8 {
        match self {
            Section::PreviousFields => SMD_CHANGE_ORIG,
            Section::ModifiedFinalFields => SMD_ALWAYS | SMD_CHANGE_NEW,
            Section::DeletedFinalFields => SMD_ALWAYS | SMD_DELETE_FINAL,
            Section::NewFields => SMD_CREATE | SMD_ALWAYS,
        }
    }
}

impl FieldMeta {
    /// Whether rippled may put this field in `section`.
    pub const fn allowed_in(&self, section: Section) -> bool {
        self.meta & section.mask() != 0
    }
}

/// rippled's field code for a field header's type code and field number.
pub const fn field_code(type_code: u16, nth: u16) -> u32 {
    ((type_code as u32) << 16) | nth as u32
}

/// The field with this code, if rippled defines one.
pub fn by_code(code: u32) -> Option<&'static FieldMeta> {
    let i = table::FIELDS.binary_search_by_key(&code, |f| f.code).ok()?;
    Some(&table::FIELDS[i])
}

/// The field with this name ("Balance"), if rippled defines one.
pub fn by_name(name: &str) -> Option<&'static FieldMeta> {
    table::FIELDS.iter().find(|f| f.name == name)
}

/// Every field, sorted by code.
pub fn all() -> &'static [FieldMeta] {
    &table::FIELDS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str) -> &'static FieldMeta {
        by_name(name).unwrap_or_else(|| panic!("no field {name}"))
    }

    #[test]
    fn the_table_is_sorted_unique_and_consistent() {
        let all = all();
        assert_eq!(all.len(), 349, "rippled 3.4.1 defines 349 fields");
        for w in all.windows(2) {
            assert!(w[0].code < w[1].code, "{} and {} are out of order or share a code", w[0].name, w[1].name);
        }
        for f in all {
            assert_eq!(f.code, field_code(f.type_code, f.nth), "{}", f.name);
            assert_eq!(by_code(f.code), Some(f), "{}", f.name);
            assert_eq!(by_name(f.name), Some(f), "{} is not unique", f.name);
        }
        assert_eq!(by_code(field_code(2, 999)), None);
        assert_eq!(by_name("NoSuchField"), None);
    }

    #[test]
    fn codes_match_the_wire_format() {
        assert_eq!(field("LedgerEntryType").code, 0x0001_0001);
        assert_eq!(field("Flags").code, 0x0002_0002);
        assert_eq!(field("Balance").code, 0x0006_0002);
        assert_eq!(field("Account").code, 0x0008_0001);
        assert_eq!(field("ModifiedNode").code, 0x000e_0005);
        assert_eq!(field("AffectedNodes").code, 0x000f_0008);
        assert_eq!(field("Balance").type_code, 6);
    }

    #[test]
    fn ordinary_fields_go_everywhere_a_change_shows() {
        for name in ["Balance", "Flags", "OwnerCount", "Sequence", "Account"] {
            let f = field(name);
            assert_eq!(f.meta, SMD_DEFAULT, "{name}");
            for section in Section::ALL {
                assert!(f.allowed_in(section), "{name} in {section:?}");
            }
        }
    }

    #[test]
    fn threading_fields_show_only_when_the_object_is_deleted() {
        for name in ["PreviousTxnID", "PreviousTxnLgrSeq"] {
            let f = field(name);
            assert_eq!(f.meta, SMD_DELETE_FINAL, "{name}");
            for section in Section::ALL {
                assert_eq!(f.allowed_in(section), section == Section::DeletedFinalFields, "{name} in {section:?}");
            }
        }
    }

    #[test]
    fn never_fields_show_nowhere_and_root_index_always_shows() {
        for name in ["LedgerEntryType", "Indexes"] {
            let f = field(name);
            assert_eq!(f.meta, SMD_NEVER, "{name}");
            assert!(Section::ALL.iter().all(|s| !f.allowed_in(*s)), "{name}");
        }
        let root = field("RootIndex");
        assert_eq!(root.meta, SMD_ALWAYS);
        for section in Section::ALL {
            assert_eq!(root.allowed_in(section), section != Section::PreviousFields, "RootIndex in {section:?}");
        }
    }

    #[test]
    fn the_other_bits_are_kept() {
        assert_eq!(field("MaximumAmount").meta, SMD_BASE_TEN | SMD_DEFAULT);
        assert_eq!(field("AMMID").meta, SMD_PSEUDO_ACCOUNT | SMD_DEFAULT);
        assert_eq!(field("AssetsTotal").meta, SMD_NEEDS_ASSET | SMD_DEFAULT);
    }
}
