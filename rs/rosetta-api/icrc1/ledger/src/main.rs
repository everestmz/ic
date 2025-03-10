use candid::candid_method;
use candid::types::number::Nat;
use ic_base_types::PrincipalId;
use ic_cdk::api::stable::{StableReader, StableWriter};
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};
use ic_icrc1::{
    endpoints::{ArchiveInfo, StandardRecord, TransferArg, TransferError, Value},
    Account, Operation, Transaction,
};
use ic_icrc1_ledger::{InitArgs, Ledger};
use ic_ledger_canister_core::ledger::{
    apply_transaction, archive_blocks, LedgerAccess, LedgerData,
};
use ic_ledger_core::{timestamp::TimeStamp, tokens::Tokens};
use num_traits::ToPrimitive;
use std::cell::RefCell;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

thread_local! {
    static LEDGER: RefCell<Option<Ledger>> = RefCell::new(None);
}

struct Access;
impl LedgerAccess for Access {
    type Ledger = Ledger;

    fn with_ledger<R>(f: impl FnOnce(&Ledger) -> R) -> R {
        LEDGER.with(|cell| {
            f(cell
                .borrow()
                .as_ref()
                .expect("ledger state not initialized"))
        })
    }

    fn with_ledger_mut<R>(f: impl FnOnce(&mut Ledger) -> R) -> R {
        LEDGER.with(|cell| {
            f(cell
                .borrow_mut()
                .as_mut()
                .expect("ledger state not initialized"))
        })
    }
}

#[init]
fn init(args: InitArgs) {
    let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
    LEDGER.with(|cell| *cell.borrow_mut() = Some(Ledger::from_init_args(args, now)))
}

#[pre_upgrade]
fn pre_upgrade() {
    Access::with_ledger(|ledger| ciborium::ser::into_writer(ledger, StableWriter::default()))
        .expect("failed to encode ledger state");
}

#[post_upgrade]
fn post_upgrade() {
    LEDGER.with(|cell| {
        *cell.borrow_mut() = Some(
            ciborium::de::from_reader(StableReader::default())
                .expect("failed to decode ledger state"),
        );
    })
}

#[query]
#[candid_method(query)]
fn icrc1_name() -> String {
    Access::with_ledger(|ledger| ledger.token_name().to_string())
}

#[query]
#[candid_method(query)]
fn icrc1_symbol() -> String {
    Access::with_ledger(|ledger| ledger.token_symbol().to_string())
}

#[query]
#[candid_method(query)]
fn icrc1_decimals() -> u8 {
    debug_assert!(ic_ledger_core::tokens::DECIMAL_PLACES <= u8::MAX as u32);
    ic_ledger_core::tokens::DECIMAL_PLACES as u8
}

#[query]
#[candid_method(query)]
fn icrc1_fee() -> Nat {
    Nat::from(Access::with_ledger(|ledger| ledger.transfer_fee()).get_e8s())
}

#[query]
#[candid_method(query)]
fn icrc1_metadata() -> Vec<(String, Value)> {
    Access::with_ledger(|ledger| ledger.metadata())
}

#[query]
#[candid_method(query)]
fn icrc1_minting_account() -> Option<Account> {
    Access::with_ledger(|ledger| Some(ledger.minting_account().clone()))
}

#[query(name = "icrc1_balance_of")]
#[candid_method(query, rename = "icrc1_balance_of")]
fn icrc1_balance_of(account: Account) -> Nat {
    Access::with_ledger(|ledger| Nat::from(ledger.balances().account_balance(&account).get_e8s()))
}

#[query(name = "icrc1_total_supply")]
#[candid_method(query, rename = "icrc1_total_supply")]
fn icrc1_total_supply() -> Nat {
    Access::with_ledger(|ledger| Nat::from(ledger.balances().total_supply().get_e8s()))
}

#[update]
#[candid_method(update)]
async fn icrc1_transfer(arg: TransferArg) -> Result<Nat, TransferError> {
    let block_idx = Access::with_ledger_mut(|ledger| {
        let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
        let created_at_time = arg
            .created_at_time
            .map(TimeStamp::from_nanos_since_unix_epoch);

        let from_account = Account {
            owner: PrincipalId::from(ic_cdk::api::caller()),
            subaccount: arg.from_subaccount,
        };

        let amount = match arg.amount.0.to_u64() {
            Some(n) => Tokens::from_e8s(n),
            None => {
                // No one can have so many tokens
                let balance = Nat::from(ledger.balances().account_balance(&from_account).get_e8s());
                assert!(balance < arg.amount);
                return Err(TransferError::InsufficientFunds { balance });
            }
        };

        let tx = if &arg.to == ledger.minting_account() {
            let expected_fee = Nat::from(0u64);
            if arg.fee.is_some() && arg.fee.as_ref() != Some(&expected_fee) {
                return Err(TransferError::BadFee { expected_fee });
            }

            let balance = ledger.balances().account_balance(&from_account);
            let min_burn_amount = ledger.transfer_fee().min(balance);
            if amount < min_burn_amount {
                return Err(TransferError::BadBurn {
                    min_burn_amount: Nat::from(min_burn_amount.get_e8s()),
                });
            }
            if amount == Tokens::ZERO {
                return Err(TransferError::BadBurn {
                    min_burn_amount: Nat::from(ledger.transfer_fee().get_e8s()),
                });
            }

            Transaction {
                operation: Operation::Burn {
                    from: from_account,
                    amount: amount.get_e8s(),
                },
                created_at_time: created_at_time.map(|t| t.as_nanos_since_unix_epoch()),
                memo: arg.memo,
            }
        } else if &from_account == ledger.minting_account() {
            let expected_fee = Nat::from(0u64);
            if arg.fee.is_some() && arg.fee.as_ref() != Some(&expected_fee) {
                return Err(TransferError::BadFee { expected_fee });
            }
            Transaction::mint(arg.to, amount, created_at_time, arg.memo)
        } else {
            let expected_fee_tokens = ledger.transfer_fee();
            let expected_fee = Nat::from(expected_fee_tokens.get_e8s());
            if arg.fee.is_some() && arg.fee.as_ref() != Some(&expected_fee) {
                return Err(TransferError::BadFee { expected_fee });
            }
            Transaction::transfer(
                from_account,
                arg.to,
                amount,
                expected_fee_tokens,
                created_at_time,
                arg.memo,
            )
        };

        let (block_idx, _) = apply_transaction(ledger, tx, now)?;
        Ok(block_idx)
    })?;

    // NB. we need to set the certified data before the first async call to make sure that the
    // blockchain state agrees with the certificate while archiving is in progress.
    ic_cdk::api::set_certified_data(&Access::with_ledger(Ledger::root_hash));

    archive_blocks::<Access>(MAX_MESSAGE_SIZE).await;
    Ok(Nat::from(block_idx))
}

#[query]
fn archives() -> Vec<ArchiveInfo> {
    Access::with_ledger(|ledger| {
        ledger
            .blockchain()
            .archive
            .read()
            .unwrap()
            .as_ref()
            .iter()
            .flat_map(|archive| {
                archive
                    .index()
                    .into_iter()
                    .map(|((start, end), canister_id)| ArchiveInfo {
                        canister_id,
                        block_range_start: Nat::from(start),
                        block_range_end: Nat::from(end),
                    })
            })
            .collect()
    })
}

#[query(name = "icrc1_supported_standards")]
#[candid_method(query, rename = "icrc1_supported_standards")]
fn supported_standards() -> Vec<StandardRecord> {
    vec![StandardRecord {
        name: "ICRC-1".to_string(),
        url: "https://github.com/dfinity/ICRC-1".to_string(),
    }]
}

fn main() {}

#[test]
fn check_candid_interface() {
    use candid::utils::{service_compatible, CandidSource};
    use std::path::PathBuf;

    candid::export_service!();

    let new_interface = __export_service();

    // check the public interface against the actual one
    let old_interface =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("icrc1.did");

    service_compatible(
        CandidSource::Text(&new_interface),
        CandidSource::File(old_interface.as_path()),
    )
    .expect("the ledger interface is not compatible with icrc1.did");
}
