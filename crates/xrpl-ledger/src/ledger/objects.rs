//! Ledger entry types — objects stored in the state SHAMap.

use serde_json::Value;

/// Ledger entry type codes (from rippled's LedgerFormats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum LedgerEntryType {
    AccountRoot = 0x0061,    // 'a'
    DirectoryNode = 0x0064,  // 'd'
    RippleState = 0x0072,    // 'r' (trust line)
    Offer = 0x006F,          // 'o'
    LedgerHashes = 0x0068,   // 'h'
    Amendments = 0x0066,     // 'f'
    FeeSettings = 0x0073,    // 's'
    Escrow = 0x0075,         // 'u'
    PayChannel = 0x0078,     // 'x'
    Check = 0x0043,          // 'C'
    DepositPreauth = 0x0070, // 'p'
    NegativeUnl = 0x004E,    // 'N'
    NFTokenPage = 0x0050,    // 'P'
    AMM = 0x0079,            // 'y'
}

impl LedgerEntryType {
    /// Look up entry type from its numeric code.
    pub fn from_type_code(code: u16) -> Option<Self> {
        match code {
            0x0061 => Some(Self::AccountRoot),
            0x0064 => Some(Self::DirectoryNode),
            0x0072 => Some(Self::RippleState),
            0x006F => Some(Self::Offer),
            0x0068 => Some(Self::LedgerHashes),
            0x0066 => Some(Self::Amendments),
            0x0073 => Some(Self::FeeSettings),
            0x0075 => Some(Self::Escrow),
            0x0078 => Some(Self::PayChannel),
            0x0043 => Some(Self::Check),
            0x0070 => Some(Self::DepositPreauth),
            0x004E => Some(Self::NegativeUnl),
            0x0050 => Some(Self::NFTokenPage),
            0x0079 => Some(Self::AMM),
            _ => None,
        }
    }

    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AccountRoot => "AccountRoot",
            Self::DirectoryNode => "DirectoryNode",
            Self::RippleState => "RippleState",
            Self::Offer => "Offer",
            Self::LedgerHashes => "LedgerHashes",
            Self::Amendments => "Amendments",
            Self::FeeSettings => "FeeSettings",
            Self::Escrow => "Escrow",
            Self::PayChannel => "PayChannel",
            Self::Check => "Check",
            Self::DepositPreauth => "DepositPreauth",
            Self::NegativeUnl => "NegativeUnl",
            Self::NFTokenPage => "NFTokenPage",
            Self::AMM => "AMM",
        }
    }
}

/// A decoded ledger object with typed accessors.
#[derive(Debug, Clone)]
pub struct LedgerObject {
    /// The decoded JSON representation.
    json: Value,
    /// The entry type (if successfully parsed).
    entry_type: Option<LedgerEntryType>,
}

impl LedgerObject {
    /// Create from a JSON value.
    pub fn from_json(json: Value) -> Self {
        let entry_type = json
            .get("LedgerEntryType")
            .and_then(|v| v.as_u64())
            .and_then(|code| LedgerEntryType::from_type_code(code as u16));

        Self { json, entry_type }
    }

    /// The entry type.
    pub fn entry_type(&self) -> Option<LedgerEntryType> {
        self.entry_type
    }

    /// The raw JSON.
    pub fn json(&self) -> &Value {
        &self.json
    }

    /// Get a string field.
    pub fn get_str(&self, field: &str) -> Option<&str> {
        self.json.get(field)?.as_str()
    }

    /// Get a u64 field.
    pub fn get_u64(&self, field: &str) -> Option<u64> {
        self.json.get(field)?.as_u64()
    }

    /// Get a u32 field.
    pub fn get_u32(&self, field: &str) -> Option<u32> {
        self.json.get(field).and_then(|v| v.as_u64()).map(|v| v as u32)
    }

    // ---- AccountRoot accessors ----

    /// Account balance in drops (AccountRoot).
    pub fn balance(&self) -> Option<u64> {
        self.json
            .get("Balance")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
    }

    /// Account sequence number (AccountRoot).
    pub fn sequence(&self) -> Option<u32> {
        self.get_u32("Sequence")
    }

    /// Owner count (AccountRoot).
    pub fn owner_count(&self) -> Option<u32> {
        self.get_u32("OwnerCount")
    }

    /// Account address string (AccountRoot).
    pub fn account(&self) -> Option<&str> {
        self.get_str("Account")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entry_type_from_code() {
        assert_eq!(LedgerEntryType::from_type_code(0x0061), Some(LedgerEntryType::AccountRoot));
        assert_eq!(LedgerEntryType::from_type_code(0x006F), Some(LedgerEntryType::Offer));
        assert_eq!(LedgerEntryType::from_type_code(0x9999), None);
    }

    #[test]
    fn ledger_object_account_root() {
        let json = json!({
            "LedgerEntryType": 97, // 0x0061
            "Account": "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh",
            "Balance": "100000000",
            "Sequence": 42,
            "OwnerCount": 3,
        });

        let obj = LedgerObject::from_json(json);
        assert_eq!(obj.entry_type(), Some(LedgerEntryType::AccountRoot));
        assert_eq!(obj.account(), Some("rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh"));
        assert_eq!(obj.balance(), Some(100_000_000));
        assert_eq!(obj.sequence(), Some(42));
        assert_eq!(obj.owner_count(), Some(3));
    }

    #[test]
    fn entry_type_names() {
        assert_eq!(LedgerEntryType::AccountRoot.name(), "AccountRoot");
        assert_eq!(LedgerEntryType::Offer.name(), "Offer");
        assert_eq!(LedgerEntryType::RippleState.name(), "RippleState");
    }
}
