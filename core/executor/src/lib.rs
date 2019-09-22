#![feature(trait_alias)]

mod adapter;
mod cycles;
mod fixed_types;
mod native_contract;
#[cfg(test)]
mod tests;
mod trie;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;

use protocol::traits::executor::contract::{AccountContract, BankContract, ContractStateAdapter};
use protocol::traits::executor::{Executor, ExecutorExecResp, InvokeContext, RcInvokeContext};
use protocol::types::{
    Address, Balance, Bloom, ContractAddress, ContractType, Fee, Hash, MerkleRoot, Receipt,
    ReceiptResult, SignedTransaction, TransactionAction, UserAddress,
};
use protocol::ProtocolResult;

use crate::adapter::{GeneralContractStateAdapter, RcGeneralContractStateAdapter};
use crate::native_contract::{
    NativeAccountContract, NativeBankContract, ACCOUNT_CONTRACT_ADDRESS, BANK_CONTRACT_ADDRESS,
};
use crate::trie::{MPTTrie, TrieDB};

pub struct TransactionExecutor<DB: TrieDB> {
    chain_id: Hash,

    trie:              MPTTrie<DB>,
    account_contract:  NativeAccountContract<GeneralContractStateAdapter<DB>>,
    bank_account:      NativeBankContract<GeneralContractStateAdapter<DB>>,
    state_adapter_map: HashMap<Address, RcGeneralContractStateAdapter<DB>>,
}

impl<DB: TrieDB> Executor for TransactionExecutor<DB> {
    fn exec(
        &mut self,
        epoch_id: u64,
        cycles_price: u64,
        coinbase: Address,
        signed_txs: Vec<SignedTransaction>,
    ) -> ProtocolResult<ExecutorExecResp> {
        let mut receipts = Vec::with_capacity(signed_txs.len());

        for signed_tx in signed_txs.into_iter() {
            let tx_hash = signed_tx.tx_hash.clone();

            let ictx = gen_invoke_ctx(
                epoch_id,
                cycles_price,
                &self.chain_id,
                &coinbase,
                &signed_tx,
            )?;

            let res = match self.dispatch(Rc::clone(&ictx), signed_tx) {
                Ok(res) => {
                    self.stash()?;
                    res
                }
                Err(e) => {
                    self.revert()?;
                    ReceiptResult::Fail {
                        system: e.to_string(),
                        user:   "".to_owned(),
                    }
                }
            };

            self.account_contract.inc_nonce(Rc::clone(&ictx))?;

            let receipt = Receipt {
                state_root: Hash::from_empty(),
                epoch_id: ictx.borrow().epoch_id,
                cycles_used: ictx.borrow().cycles_used.clone(),
                result: res,
                tx_hash,
            };
            receipts.push(receipt);
        }

        //  Calculate the total fee and reward `coinbsae`
        let mut all_cycles_used: Vec<Fee> = vec![];
        for receipt in receipts.iter() {
            modify_all_cycles_used(&mut all_cycles_used, &receipt.cycles_used);
        }
        for cycles_used in all_cycles_used.iter() {
            self.account_contract.add_balance(
                &cycles_used.asset_id,
                &coinbase,
                Balance::from(cycles_used.cycle),
            )?;
        }

        // commit state
        let state_root = self.commit()?;
        for receipt in receipts.iter_mut() {
            receipt.state_root = state_root.clone();
        }

        Ok(ExecutorExecResp {
            receipts,
            all_cycles_used,
            logs_bloom: Bloom::default(),
            state_root: state_root.clone(),
        })
    }
}

impl<DB: TrieDB> TransactionExecutor<DB> {
    pub fn new(chain_id: Hash, db: Arc<DB>) -> ProtocolResult<Self> {
        let mut trie = MPTTrie::new(Arc::clone(&db));
        let root = trie.commit()?;

        Self::from(chain_id, root, Arc::clone(&db))
    }

    pub fn from(chain_id: Hash, state_root: MerkleRoot, db: Arc<DB>) -> ProtocolResult<Self> {
        let trie = MPTTrie::from(state_root.clone(), Arc::clone(&db))?;

        let mut state_adapter_map = HashMap::new();

        // gen account contract
        let account_state_adapter =
            gen_contract_state(&trie, &ACCOUNT_CONTRACT_ADDRESS, Arc::clone(&db))?;
        let account_contract = NativeAccountContract::new(Rc::clone(&account_state_adapter));
        state_adapter_map.insert(
            ACCOUNT_CONTRACT_ADDRESS.clone(),
            Rc::clone(&account_state_adapter),
        );

        // gen bank contract
        let bank_state_adapter =
            gen_contract_state(&trie, &BANK_CONTRACT_ADDRESS, Arc::clone(&db))?;
        let bank_account =
            NativeBankContract::new(chain_id.clone(), Rc::clone(&bank_state_adapter));
        state_adapter_map.insert(
            BANK_CONTRACT_ADDRESS.clone(),
            Rc::clone(&bank_state_adapter),
        );

        Ok(Self {
            chain_id,
            trie,
            account_contract,
            bank_account,
            state_adapter_map,
        })
    }

    fn dispatch(
        &mut self,
        ictx: RcInvokeContext,
        signed_tx: SignedTransaction,
    ) -> ProtocolResult<ReceiptResult> {
        let action = &signed_tx.raw.action;

        let res = match action {
            TransactionAction::Transfer { receiver, .. } => {
                let to = &Address::User(receiver.clone());
                self.handle_transfer(Rc::clone(&ictx), &to)?
            }
            TransactionAction::Deploy {
                code,
                contract_type,
            } => self.handle_deploy(Rc::clone(&ictx), code, contract_type)?,
            _ => panic!("Unsupported transaction"),
        };

        Ok(res)
    }

    fn handle_transfer(
        &mut self,
        ictx: RcInvokeContext,
        to: &Address,
    ) -> ProtocolResult<ReceiptResult> {
        let from = &ictx.borrow().caller;
        let carrying_asset = ictx
            .borrow()
            .carrying_asset
            .clone()
            .expect("in transfer, `carrying_asset` cannot be empty");

        // check asset exists
        self.bank_account
            .get_asset(Rc::clone(&ictx), &carrying_asset.asset_id)?;

        let before_amount = self
            .account_contract
            .get_balance(&carrying_asset.asset_id, &from)?;

        self.account_contract.transfer(Rc::clone(&ictx), &to)?;

        let after_amount = self
            .account_contract
            .get_balance(&carrying_asset.asset_id, &from)?;

        Ok(ReceiptResult::Transfer {
            receiver: UserAddress::from_bytes(to.as_bytes())?,
            asset_id: carrying_asset.asset_id.clone(),
            before_amount,
            after_amount,
        })
    }

    fn handle_deploy(
        &mut self,
        ictx: RcInvokeContext,
        code: &Bytes,
        contract_type: &ContractType,
    ) -> ProtocolResult<ReceiptResult> {
        match contract_type {
            ContractType::Asset => {
                // TODO(@yejiayu): Check account balance?
                let nonce = self.account_contract.get_nonce(&ictx.borrow().caller)?;
                let address = ContractAddress::from_code(code.clone(), nonce, ContractType::Asset)?;

                self.bank_account.register(
                    Rc::clone(&ictx),
                    &address,
                    "muta-token".to_owned(),
                    "MTT".to_owned(),
                    Balance::from(21_000_000u64),
                )?;

                Ok(ReceiptResult::Deploy {
                    contract:      address,
                    contract_type: ContractType::Asset,
                })
            }
            _ => panic!("Unsupported transaction"),
        }
    }

    fn stash(&mut self) -> ProtocolResult<()> {
        for (_, state) in self.state_adapter_map.iter() {
            state.borrow_mut().stash()?;
        }
        Ok(())
    }

    fn revert(&mut self) -> ProtocolResult<()> {
        for (_, state) in self.state_adapter_map.iter() {
            state.borrow_mut().revert_cache()?;
        }
        Ok(())
    }

    fn commit(&mut self) -> ProtocolResult<MerkleRoot> {
        for (address, state) in self.state_adapter_map.iter() {
            let root = state.borrow_mut().commit()?;

            self.trie.insert(address.as_bytes(), root.as_bytes())?;
        }

        self.trie.commit()
    }
}

fn gen_contract_state<DB: TrieDB>(
    trie: &MPTTrie<DB>,
    address: &Address,
    db: Arc<DB>,
) -> ProtocolResult<RcGeneralContractStateAdapter<DB>> {
    let trie = {
        if let Some(val) = trie.get(&address.as_bytes())? {
            let contract_root = MerkleRoot::from_bytes(val)?;
            MPTTrie::from(contract_root, db)?
        } else {
            MPTTrie::new(db)
        }
    };

    let state_adapter = GeneralContractStateAdapter::new(trie);
    Ok(Rc::new(RefCell::new(state_adapter)))
}

fn modify_all_cycles_used(all_cycles_used: &mut Vec<Fee>, cycles_used: &Fee) {
    for fee in all_cycles_used.iter_mut() {
        if fee.asset_id == cycles_used.asset_id {
            fee.cycle += cycles_used.cycle;
            return;
        }
    }

    let new_fee = Fee {
        asset_id: cycles_used.asset_id.clone(),
        cycle:    cycles_used.cycle,
    };

    all_cycles_used.push(new_fee);
}

fn gen_invoke_ctx(
    epoch_id: u64,
    cycles_price: u64,
    chain_id: &Hash,
    coinbase: &Address,
    signed_tx: &SignedTransaction,
) -> ProtocolResult<RcInvokeContext> {
    let carrying_asset = match &signed_tx.raw.action {
        TransactionAction::Transfer { carrying_asset, .. } => Some(carrying_asset.clone()),
        TransactionAction::Call { carrying_asset, .. } => carrying_asset.clone(),
        _ => None,
    };

    let ctx = InvokeContext {
        chain_id: chain_id.clone(),
        cycles_used: Fee {
            asset_id: signed_tx.raw.fee.asset_id.clone(),
            cycle:    0,
        },
        cycles_limit: signed_tx.raw.fee.clone(),
        caller: Address::User(UserAddress::from_pubkey_bytes(signed_tx.pubkey.clone())?),
        coinbase: coinbase.clone(),
        epoch_id,
        cycles_price,
        carrying_asset,
    };
    Ok(Rc::new(RefCell::new(ctx)))
}