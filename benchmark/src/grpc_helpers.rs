// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use admission_control_proto::proto::{
    admission_control::{
        AdmissionControlStatusCode, SubmitTransactionRequest,
        SubmitTransactionResponse as ProtoSubmitTransactionResponse,
    },
    admission_control_grpc::AdmissionControlClient,
};
use client::AccountStatus;
use failure::prelude::*;
use futures::{
    stream::{self, Stream},
    Future,
};
use grpcio::{self, CallOption};
use logger::prelude::*;
use proto_conv::{FromProto, IntoProto};
use std::{collections::HashMap, slice::Chunks, thread, time};
use types::{
    account_address::AccountAddress,
    account_config::get_account_resource_or_default,
    get_with_proof::{RequestItem, ResponseItem, UpdateToLatestLedgerRequest},
};

use crate::OP_COUNTER;

/// Timeout duration for grpc call option.
const GRPC_TIMEOUT_MS: u64 = 8_000;
/// Duration to sleep between consecutive queries for accounts' sequence numbers.
const QUERY_SEQUENCE_NUMBERS_INTERVAL_US: u64 = 100;
/// Max number of iterations to wait (using accounts' sequence number) for submitted
/// TXNs to become committed.
pub const MAX_WAIT_COMMIT_ITERATIONS: u64 = 10_000;

/// Return a parameter that controls how "patient" AC clients are,
/// who are waiting the response from AC for this amount of time.
fn get_default_grpc_call_option() -> CallOption {
    CallOption::default()
        .wait_for_ready(true)
        .timeout(std::time::Duration::from_millis(GRPC_TIMEOUT_MS))
}

/// Divide generic items into a vector of chunks of nearly equal size.
pub fn divide_items<T>(items: &[T], num_chunks: usize) -> Chunks<T> {
    let chunk_size = if (num_chunks == 0) || (items.len() / num_chunks == 0) {
        std::cmp::max(1, items.len())
    } else {
        items.len() / num_chunks
    };
    items.chunks(chunk_size)
}

/// ---------------------------------------------------------- ///
///  Transaction async request and response handling helpers.  ///
/// ---------------------------------------------------------- ///

/// By checking 1) ac status, 2) vm status, and 3) mempool status, decide whether the reponse
/// from AC is accepted. If not, classify what the error type is.
fn check_ac_response(resp: &ProtoSubmitTransactionResponse) -> bool {
    if resp.has_ac_status() {
        let status = resp.get_ac_status().get_code();
        if status == AdmissionControlStatusCode::Accepted {
            OP_COUNTER.inc(&format!("submit_txns.{:?}", status));
            true
        } else {
            OP_COUNTER.inc(&format!("submit_txns.{:?}", status));
            error!("Request rejected by AC: {:?}", resp);
            false
        }
    } else if resp.has_vm_status() {
        OP_COUNTER.inc(&format!("submit_txns.{:?}", resp.get_vm_status()));
        error!("Request causes error on VM: {:?}", resp);
        false
    } else if resp.has_mempool_status() {
        OP_COUNTER.inc(&format!(
            "submit_txns.{:?}",
            resp.get_mempool_status().get_code()
        ));
        error!("Request causes error on mempool: {:?}", resp);
        false
    } else {
        OP_COUNTER.inc("submit_txns.Unknown");
        error!("Request rejected by AC for unknown error: {:?}", resp);
        false
    }
}

/// Send TXN requests to AC async, wait for and check the responses from AC.
/// Return the responses of only accepted TXN requests.
/// Ignore but count both gRPC-failed submissions and AC-rejected TXNs.
pub fn submit_and_wait_txn_requests(
    client: &AdmissionControlClient,
    txn_requests: &[SubmitTransactionRequest],
) -> Vec<ProtoSubmitTransactionResponse> {
    let futures: Vec<_> = txn_requests
        .iter()
        .filter_map(|req| {
            match client.submit_transaction_async_opt(&req, get_default_grpc_call_option()) {
                Ok(future) => Some(future),
                Err(e) => {
                    OP_COUNTER.inc(&format!("submit_txns.{:?}", e));
                    error!("Failed to send gRPC request: {:?}", e);
                    None
                }
            }
        })
        .collect();
    // Wait all the futures unorderedly, then pick only accepted responses.
    stream::futures_unordered(futures)
        .wait()
        .filter_map(|future_result| match future_result {
            Ok(proto_resp) => {
                if check_ac_response(&proto_resp) {
                    Some(proto_resp)
                } else {
                    None
                }
            }
            Err(e) => {
                OP_COUNTER.inc(&format!("submit_txns.{:?}", e));
                error!("Failed to receive gRPC response: {:?}", e);
                None
            }
        })
        .collect()
}

/// ------------------------------------------------------------ ///
///  Account state async request and response handling helpers.  ///
/// ------------------------------------------------------------ ///

/// Send account state request async with a AC client.
/// Try to unmarshall only the first ResponseItem in the succeeded response.
/// Return a tuple consisting of address (as account's identifier), and deserialized response item.
fn get_account_state_async(
    client: &AdmissionControlClient,
    address: AccountAddress,
) -> Result<impl Future<Item = (AccountAddress, ResponseItem), Error = failure::Error>> {
    let requested_item = RequestItem::GetAccountState { address };
    let requested_items = vec![requested_item];
    let req = UpdateToLatestLedgerRequest::new(0, requested_items);
    let proto_req = req.into_proto();
    let ret = client
        .update_to_latest_ledger_async_opt(&proto_req, get_default_grpc_call_option())?
        .then(move |account_state_proof_resp| {
            // Instead of convert entire account_state_proof_resp to UpdateToLatestLedgerResponse,
            // directly get the ResponseItems and convert only first item to rust struct.
            let mut response_items = account_state_proof_resp?.take_response_items();
            // Directly call response_items.remove(0) may panic, which is not what we want.
            if response_items.is_empty() {
                bail!("Failed to get first item from empty ResponseItem array")
            } else {
                let response_item = ResponseItem::from_proto(response_items.remove(0))?;
                Ok((address, response_item))
            }
        });
    Ok(ret)
}

/// Process valid ResponseItem to return account's sequence number and status.
fn handle_account_state_response(resp: ResponseItem) -> Result<(u64, AccountStatus)> {
    let account_state_proof = resp.into_get_account_state_response()?;
    if let Some(account_state_blob) = account_state_proof.blob {
        let account_resource = get_account_resource_or_default(&Some(account_state_blob))?;
        Ok((account_resource.sequence_number(), AccountStatus::Persisted))
    } else {
        bail!("failed to get account state because account doesn't exist")
    }
}

/// Request a bunch of accounts' states, including sequence numbers and status from validator.
/// Ignore any failure, during either requesting or processing, and continue for next account.
/// Return the mapping from address to (sequence number, account status) tuple
/// for all successfully requested accounts.
pub fn get_account_states(
    client: &AdmissionControlClient,
    addresses: &[AccountAddress],
) -> HashMap<AccountAddress, (u64, AccountStatus)> {
    let futures: Vec<_> = addresses
        .iter()
        .filter_map(|address| match get_account_state_async(client, *address) {
            Ok(future) => Some(future),
            Err(e) => {
                error!("Failed to send account request: {:?}", e);
                None
            }
        })
        .collect();
    let future_stream = stream::futures_unordered(futures);
    // Collect successfully requested account states.
    let mut states = HashMap::new();
    for pair_result in future_stream.wait() {
        match pair_result {
            Ok((address, future_resp)) => match handle_account_state_response(future_resp) {
                Ok((sequence_number, status)) => {
                    debug!(
                        "Update {:?}'s sequence number to {:?}",
                        address, sequence_number
                    );
                    states.insert(address, (sequence_number, status));
                }
                Err(e) => {
                    error!("Invalid account response for {:?}: {:?}", address, e);
                }
            },
            Err(e) => {
                error!("Failed to receive account response: {:?}", e);
            }
        }
    }
    states
}

/// For each sender account, synchronize its persisted sequence number from validator.
/// When this sync sequence number equals the account's local sequence number,
/// all its transactions are committed. Timeout if such condition is never met for all senders.
/// Return sender accounts' most recent persisted sequence numbers.
pub fn sync_account_sequence_number(
    client: &AdmissionControlClient,
    senders_and_sequence_numbers: &[(AccountAddress, u64)],
) -> HashMap<AccountAddress, u64> {
    // Invariants for the keys in targets (T), unfinished (U) and finished (F):
    // (1) T = U union F, and (2) U and F are disjoint.
    let targets: HashMap<AccountAddress, u64> =
        senders_and_sequence_numbers.iter().cloned().collect();
    let mut unfinished: HashMap<AccountAddress, u64> = senders_and_sequence_numbers
        .iter()
        .map(|(sender, _)| (*sender, 0))
        .collect();
    let mut finished = HashMap::new();

    let mut num_iters = 0;
    while num_iters < MAX_WAIT_COMMIT_ITERATIONS {
        let unfinished_addresses: Vec<_> = unfinished.keys().copied().collect();
        let states = get_account_states(client, &unfinished_addresses);
        for (address, (sequence_number, _status)) in states.iter() {
            if let Some(target) = targets.get(address) {
                if sequence_number == target {
                    debug!("All TXNs from {:?} are committed", address);
                    finished.insert(*address, *sequence_number);
                    unfinished.remove(address);
                } else {
                    debug!(
                        "{} TXNs from {:?} still uncommitted",
                        target - sequence_number,
                        address
                    );
                    unfinished.insert(*address, *sequence_number);
                }
            }
        }
        if finished.len() == senders_and_sequence_numbers.len() {
            break;
        }
        thread::sleep(time::Duration::from_micros(
            QUERY_SEQUENCE_NUMBERS_INTERVAL_US,
        ));
        num_iters += 1;
    }
    // Merging won't have conflict because F and U are disjoint.
    finished.extend(unfinished);
    finished
}

#[cfg(test)]
mod tests {
    use crate::divide_items;

    #[test]
    fn test_divide_items() {
        let items: Vec<_> = (0..4).collect();
        let mut iter1 = divide_items(&items, 3);
        assert_eq!(iter1.next().unwrap(), &[0]);
        assert_eq!(iter1.next().unwrap(), &[1]);
        assert_eq!(iter1.next().unwrap(), &[2]);
        assert_eq!(iter1.next().unwrap(), &[3]);

        let mut iter2 = divide_items(&items, 2);
        assert_eq!(iter2.next().unwrap(), &[0, 1]);
        assert_eq!(iter2.next().unwrap(), &[2, 3]);

        let mut iter3 = divide_items(&items, 0);
        assert_eq!(iter3.next().unwrap(), &[0, 1, 2, 3]);

        let empty_slice: Vec<u32> = vec![];
        let mut empty_iter = divide_items(&empty_slice, 3);
        assert!(empty_iter.next().is_none());
        let mut empty_iter = divide_items(&empty_slice, 0);
        assert!(empty_iter.next().is_none());
    }
}
