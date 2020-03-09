use blockchain_traits::TransactionOutcome;
use oasis_types::{AccountMeta, Address, Event};

use crate::{output::Receipt, State};

#[derive(Debug)]
pub struct PendingTransaction<'bc> {
    pub caller: Address,
    pub callee: Address,
    pub value: u128,
    pub state: State<'bc>,
    pub input: Vec<u8>,
    pub outcome: TransactionOutcome,
    pub output: Vec<u8>,
    pub events: Vec<Event>,
    pub gas_left: u64,
    pub base_gas: u64,
}

impl<'bc> blockchain_traits::PendingTransaction for PendingTransaction<'bc> {
    fn address(&self) -> &Address {
        &self.callee
    }

    fn sender(&self) -> &Address {
        &self.caller
    }

    fn value(&self) -> u128 {
        self.value
    }

    fn input(&self) -> &[u8] {
        self.input.as_slice()
    }

    fn create(&mut self, _value: u128, _code: &[u8]) -> Box<dyn blockchain_traits::Receipt> {
        unimplemented!();
    }

    fn transact(
        &mut self,
        callee: Address,
        value: u128,
        input: &[u8],
    ) -> Box<dyn blockchain_traits::Receipt> {
        let caller = self.callee;
        let mut receipt = Receipt {
            caller,
            callee,
            value,
            gas_used: 0, // TODO(#116)
            output: Vec::new(),
            events: Vec::new(),
            outcome: TransactionOutcome::Success,
        };

        if self.gas_left < self.base_gas {
            receipt.outcome = TransactionOutcome::InsufficientGas;
            return box receipt;
        }

        if !self.state.contains_key(&callee) {
            receipt.outcome = TransactionOutcome::InvalidCallee;
            return box receipt;
        }

        let mut ptx_state = self.state.clone();

        let caller_acct = ptx_state.get_mut(&caller).unwrap().to_mut();

        if caller_acct.balance < value {
            receipt.outcome = TransactionOutcome::InsufficientFunds;
            return box receipt;
        } else {
            caller_acct.balance -= value
        }

        ptx_state.get_mut(&callee).unwrap().to_mut().balance += value;

        let mut pending_transaction = PendingTransaction {
            caller: self.callee,
            callee,
            value,
            input: input.to_vec(),
            outcome: TransactionOutcome::Success,
            state: ptx_state,
            events: Vec::new(),
            output: Vec::new(),
            base_gas: self.base_gas,
            gas_left: self.gas_left - self.base_gas,
        };

        if let Some(main) = self.state.get(&callee).unwrap().main {
            let ptx: &mut dyn blockchain_traits::PendingTransaction = &mut pending_transaction;
            let errno = main(unsafe {
                // Extend the lifetime, as required by the FFI type.
                // This is only unsafe if the `main` fn stores the pointer,
                // but this is disallowed by the precondition on `main`.
                &(std::mem::transmute::<&mut _, &'static mut _>(ptx) as *mut _) as *const _
            });
            if errno != 0 {
                pending_transaction.outcome = TransactionOutcome::Aborted;
            }
        }

        receipt.outcome = pending_transaction.outcome;
        receipt.output = pending_transaction.output;
        if blockchain_traits::Receipt::reverted(&receipt) {
            receipt.events.clear();
        } else {
            self.state = pending_transaction.state;
            receipt
                .events
                .append(&mut pending_transaction.events.clone());
            self.events.append(&mut pending_transaction.events);
        }
        box receipt
    }

    fn ret(&mut self, data: &[u8]) {
        assert!(self.output.is_empty());
        self.output = data.to_vec()
    }

    fn err(&mut self, data: &[u8]) {
        assert!(self.output.is_empty());
        self.output = data.to_vec();
        self.outcome = TransactionOutcome::Aborted;
    }

    fn emit(&mut self, topics: &[&[u8]], data: &[u8]) {
        self.events.push(Event {
            emitter: self.callee,
            topics: topics
                .iter()
                .map(|t| {
                    let mut t_arr = [0u8; 32];
                    let topic_len = std::cmp::min(t.len(), 32);
                    t_arr[..topic_len].copy_from_slice(&t[..topic_len]);
                    t_arr
                })
                .collect(),
            data: data.to_vec(),
        });
    }

    fn state(&self) -> &dyn blockchain_traits::KVStore {
        self.state.get(&self.callee).map(|acct| &**acct).unwrap()
    }

    fn state_mut(&mut self) -> &mut dyn blockchain_traits::KVStoreMut {
        self.state
            .get_mut(&self.callee)
            .map(std::borrow::Cow::to_mut)
            .unwrap()
    }

    fn code_at(&self, addr: &Address) -> Option<&[u8]> {
        self.state.get(addr).map(|acct| acct.code.as_ref())
    }

    fn account_meta_at(&self, addr: &Address) -> Option<AccountMeta> {
        self.state.get(addr).map(|acct| AccountMeta {
            balance: acct.balance,
            expiry: acct.expiry,
        })
    }
}
