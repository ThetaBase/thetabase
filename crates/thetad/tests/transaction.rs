//! Several writes that land as one commit, or not at all.
//!
//! # What these tests are actually for
//!
//! `put_many` batches the network and says so in its own doc comment: each
//! write is its own commit. That is fine and it is not a transaction, and the
//! two look identical from the call site — which is the whole reason this
//! exists. A caller recording a payment and marking the invoice paid needs both
//! or neither, and `a_refused_transaction_leaves_nothing_behind` is the test
//! that says so.
//!
//! The load-bearing pair is that one and
//! `a_transaction_lands_as_a_single_commit`. Between them they pin the property
//! from both sides: everything arrives together, and on refusal nothing
//! arrives. A test that only checked the happy path would pass against an
//! implementation that applied operations one at a time.

use theta_proto::wire::{Precondition, RequestBody, ResponseBody, TxAction, TxOp};

mod harness;
use harness::{json, Client, Harness};

fn put(key: &str, value: i64, expect: Option<Precondition>) -> TxOp {
    TxOp {
        key: key.into(),
        expect,
        action: TxAction::Put {
            value_json: json(value),
            ttl: 0,
        },
    }
}

async fn read(client: &mut Client, key: &str) -> Option<i64> {
    match client.call(RequestBody::Get { key: key.into() }).await {
        ResponseBody::Get {
            found, value_json, ..
        } => found.then(|| {
            match serde_json::from_str::<theta_core::Value>(&value_json).expect("a value") {
                theta_core::Value::Int(n) => n,
                other => panic!("expected an integer, got {other:?}"),
            }
        }),
        other => panic!("unexpected response: {other:?}"),
    }
}

async fn version_of(client: &mut Client, key: &str) -> u64 {
    match client.call(RequestBody::Get { key: key.into() }).await {
        ResponseBody::Get { version_id, .. } => version_id,
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn a_transaction_lands_as_a_single_commit() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let response = client
        .call(RequestBody::Transaction {
            ops: vec![put("payment/1", 500, None), put("invoice/1", 1, None)],
        })
        .await;

    let commit = match response {
        ResponseBody::Transaction { commit_id } => commit_id,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(!commit.is_empty(), "no commit id came back");

    assert_eq!(read(&mut client, "payment/1").await, Some(500));
    assert_eq!(read(&mut client, "invoice/1").await, Some(1));

    // One commit, not two. Both rows were written by the same entry, so they
    // carry the same version — which is the observable difference between a
    // transaction and a batch of writes, and the thing a caller can check.
    assert_eq!(
        version_of(&mut client, "payment/1").await,
        version_of(&mut client, "invoice/1").await,
        "the two rows are at different versions, so they were written by \
         different commits and this was a batch rather than a transaction"
    );
}

#[tokio::test]
async fn a_refused_transaction_leaves_nothing_behind() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    // The invoice already exists — somebody billed this period already.
    client
        .call(RequestBody::Put {
            key: "invoice/2".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;

    // Record the payment and create the invoice, the second conditional on the
    // invoice not existing. The condition fails, so neither must happen.
    let response = client
        .call(RequestBody::Transaction {
            ops: vec![
                put("payment/2", 500, None),
                put("invoice/2", 2, Some(Precondition::Absent)),
            ],
        })
        .await;

    assert!(
        matches!(response, ResponseBody::PreconditionFailed { .. }),
        "expected the precondition to be reported, got {response:?}"
    );

    // The operation with no condition on it must not have landed either. This
    // is the assertion the whole file exists for: an implementation that
    // applied operations as it walked them would have written the payment
    // before reaching the invoice, and the customer would have a payment
    // against an invoice that was never raised.
    assert_eq!(
        read(&mut client, "payment/2").await,
        None,
        "the unconditional operation landed even though the transaction was \
         refused, so operations are being applied before every condition is \
         checked"
    );

    // And the existing row is untouched.
    assert_eq!(read(&mut client, "invoice/2").await, Some(1));
}

#[tokio::test]
async fn a_precondition_is_checked_against_the_branch_and_not_against_earlier_operations() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    // `b` does not exist. A transaction writing `a` then requiring `b` absent
    // must succeed: nothing in the transaction created `b`.
    let response = client
        .call(RequestBody::Transaction {
            ops: vec![put("a", 1, None), put("b", 2, Some(Precondition::Absent))],
        })
        .await;

    assert!(
        matches!(response, ResponseBody::Transaction { .. }),
        "expected the transaction to commit, got {response:?}"
    );
    assert_eq!(read(&mut client, "b").await, Some(2));
}

#[tokio::test]
async fn an_empty_transaction_is_refused_rather_than_committed() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let response = client.call(RequestBody::Transaction { ops: vec![] }).await;

    // Committing nothing while handing back a commit id would hide a caller bug
    // behind something that looks like work.
    assert!(
        matches!(response, ResponseBody::Error(_)),
        "an empty transaction was accepted: {response:?}"
    );
}

#[tokio::test]
async fn one_key_twice_in_a_transaction_is_refused() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let response = client
        .call(RequestBody::Transaction {
            ops: vec![put("k", 1, None), put("k", 2, Some(Precondition::Absent))],
        })
        .await;

    // The second condition would be evaluated against a state the first
    // operation is about to replace. There is no reading of that a caller would
    // find unsurprising, so it is refused rather than resolved.
    assert!(
        matches!(response, ResponseBody::Error(_)),
        "a transaction touching one key twice was accepted: {response:?}"
    );
    assert_eq!(
        read(&mut client, "k").await,
        None,
        "it wrote something anyway"
    );
}

#[tokio::test]
async fn a_transaction_can_delete_and_write_together() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "old".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;

    let response = client
        .call(RequestBody::Transaction {
            ops: vec![
                TxOp {
                    key: "old".into(),
                    expect: None,
                    action: TxAction::Delete,
                },
                put("new", 2, None),
            ],
        })
        .await;

    assert!(
        matches!(response, ResponseBody::Transaction { .. }),
        "expected the transaction to commit, got {response:?}"
    );
    assert_eq!(read(&mut client, "old").await, None);
    assert_eq!(read(&mut client, "new").await, Some(2));
}
