#[macro_use]
extern crate derive_more;
#[macro_use]
extern crate serde;

mod address;
mod balance;

pub use address::Address;
pub use balance::Balance;

#[repr(C)]
#[derive(PartialEq, Eq)]
#[doc(hidden)]
pub struct ExtStatusCode(pub u32);

#[allow(non_upper_case_globals)] // it's supposed to be a non-exhaustive enum
impl ExtStatusCode {
    pub const Success: ExtStatusCode = ExtStatusCode(0);
    pub const InsufficientFunds: ExtStatusCode = ExtStatusCode(1);
    pub const InvalidInput: ExtStatusCode = ExtStatusCode(2);
    pub const NoAccount: ExtStatusCode = ExtStatusCode(3);
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct AccountMeta {
    pub balance: u128,
    pub expiry: Option<std::time::Duration>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Event {
    pub emitter: Address,
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
}
