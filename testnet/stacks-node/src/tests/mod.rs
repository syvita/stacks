use std::convert::TryInto;

use rand::RngCore;

use stacks::chainstate::burn::ConsensusHash;
use stacks::chainstate::stacks::events::{STXEventType, StacksTransactionEvent};
use stacks::chainstate::stacks::{
    db::StacksChainState, miner::BlockBuilderSettings, miner::StacksMicroblockBuilder,
    CoinbasePayload, StacksBlock, StacksMicroblock, StacksPrivateKey, StacksPublicKey,
    StacksTransaction, StacksTransactionSigner, TokenTransferMemo, TransactionAnchorMode,
    TransactionAuth, TransactionContractCall, TransactionPayload, TransactionPostConditionMode,
    TransactionSmartContract, TransactionSpendingCondition, TransactionVersion,
    C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
};
use stacks::codec::StacksMessageCodec;
use stacks::core::CHAIN_ID_TESTNET;
use stacks::types::chainstate::{StacksAddress, StacksMicroblockHeader};
use stacks::util::get_epoch_time_secs;
use stacks::util::hash::hex_bytes;
use stacks::util::strings::StacksString;
use stacks::vm::costs::ExecutionCost;
use stacks::vm::database::BurnStateDB;
use stacks::vm::types::PrincipalData;
use stacks::vm::{ClarityName, ContractName, Value};
use stacks::{address::AddressHashMode, util::hash::to_hex};

use crate::helium::RunLoop;
use stacks::core::StacksEpochId;

use super::burnchains::bitcoin_regtest_controller::ParsedUTXO;
use super::Config;

mod atlas;
mod bitcoin_regtest;
mod epoch_205;
mod integrations;
mod mempool;
pub mod neon_integrations;

// $ cat /tmp/out.clar
pub const STORE_CONTRACT: &str = r#"(define-map store { key: (string-ascii 32) } { value: (string-ascii 32) })
 (define-public (get-value (key (string-ascii 32)))
    (begin
      (print (concat "Getting key " key))
      (match (map-get? store { key: key })
        entry (ok (get value entry))
        (err 0))))
 (define-public (set-value (key (string-ascii 32)) (value (string-ascii 32)))
    (begin
        (print (concat "Setting key " key))
        (map-set store { key: key } { value: value })
        (ok true)))"#;
// ./blockstack-cli --testnet publish 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 0 0 store /tmp/out.clar

pub const SK_1: &'static str = "a1289f6438855da7decf9b61b852c882c398cff1446b2a0f823538aa2ebef92e01";
pub const SK_2: &'static str = "4ce9a8f7539ea93753a36405b16e8b57e15a552430410709c2b6d65dca5c02e201";
pub const SK_3: &'static str = "cb95ddd0fe18ec57f4f3533b95ae564b3f1ae063dbf75b46334bd86245aef78501";

pub const ADDR_4: &'static str = "ST31DA6FTSJX2WGTZ69SFY11BH51NZMB0ZZ239N96";

lazy_static! {
    pub static ref PUBLISH_CONTRACT: Vec<u8> = make_contract_publish(
        &StacksPrivateKey::from_hex(
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3"
        )
        .unwrap(),
        0,
        10,
        "store",
        STORE_CONTRACT
    );
}

pub fn serialize_sign_sponsored_sig_tx_anchor_mode_version(
    payload: TransactionPayload,
    sender: &StacksPrivateKey,
    payer: &StacksPrivateKey,
    sender_nonce: u64,
    payer_nonce: u64,
    tx_fee: u64,
    anchor_mode: TransactionAnchorMode,
    version: TransactionVersion,
) -> Vec<u8> {
    serialize_sign_tx_anchor_mode_version(
        payload,
        sender,
        Some(payer),
        sender_nonce,
        Some(payer_nonce),
        tx_fee,
        anchor_mode,
        version,
    )
}

pub fn serialize_sign_standard_single_sig_tx(
    payload: TransactionPayload,
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
) -> Vec<u8> {
    serialize_sign_standard_single_sig_tx_anchor_mode(
        payload,
        sender,
        nonce,
        tx_fee,
        TransactionAnchorMode::OnChainOnly,
    )
}

pub fn serialize_sign_standard_single_sig_tx_anchor_mode(
    payload: TransactionPayload,
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    anchor_mode: TransactionAnchorMode,
) -> Vec<u8> {
    serialize_sign_standard_single_sig_tx_anchor_mode_version(
        payload,
        sender,
        nonce,
        tx_fee,
        anchor_mode,
        TransactionVersion::Testnet,
    )
}

pub fn serialize_sign_standard_single_sig_tx_anchor_mode_version(
    payload: TransactionPayload,
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    anchor_mode: TransactionAnchorMode,
    version: TransactionVersion,
) -> Vec<u8> {
    serialize_sign_tx_anchor_mode_version(
        payload,
        sender,
        None,
        nonce,
        None,
        tx_fee,
        anchor_mode,
        version,
    )
}

pub fn serialize_sign_tx_anchor_mode_version(
    payload: TransactionPayload,
    sender: &StacksPrivateKey,
    payer: Option<&StacksPrivateKey>,
    sender_nonce: u64,
    payer_nonce: Option<u64>,
    tx_fee: u64,
    anchor_mode: TransactionAnchorMode,
    version: TransactionVersion,
) -> Vec<u8> {
    let mut sender_spending_condition =
        TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(sender))
            .expect("Failed to create p2pkh spending condition from public key.");
    sender_spending_condition.set_nonce(sender_nonce);

    let auth = match (payer, payer_nonce) {
        (Some(payer), Some(payer_nonce)) => {
            let mut payer_spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(
                StacksPublicKey::from_private(payer),
            )
            .expect("Failed to create p2pkh spending condition from public key.");
            payer_spending_condition.set_nonce(payer_nonce);
            payer_spending_condition.set_tx_fee(tx_fee);
            TransactionAuth::Sponsored(sender_spending_condition, payer_spending_condition)
        }
        _ => {
            sender_spending_condition.set_tx_fee(tx_fee);
            TransactionAuth::Standard(sender_spending_condition)
        }
    };
    let mut unsigned_tx = StacksTransaction::new(version, auth, payload);
    unsigned_tx.anchor_mode = anchor_mode;
    unsigned_tx.post_condition_mode = TransactionPostConditionMode::Allow;
    unsigned_tx.chain_id = CHAIN_ID_TESTNET;

    let mut tx_signer = StacksTransactionSigner::new(&unsigned_tx);
    tx_signer.sign_origin(sender).unwrap();
    if let (Some(payer), Some(_)) = (payer, payer_nonce) {
        tx_signer.sign_sponsor(payer).unwrap();
    }

    let mut buf = vec![];
    tx_signer
        .get_tx()
        .unwrap()
        .consensus_serialize(&mut buf)
        .unwrap();
    buf
}

pub fn make_contract_publish(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    contract_name: &str,
    contract_content: &str,
) -> Vec<u8> {
    let name = ContractName::from(contract_name);
    let code_body = StacksString::from_string(&contract_content.to_string()).unwrap();

    let payload = TransactionSmartContract { name, code_body };

    serialize_sign_standard_single_sig_tx(payload.into(), sender, nonce, tx_fee)
}

pub fn make_contract_publish_microblock_only(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    contract_name: &str,
    contract_content: &str,
) -> Vec<u8> {
    let name = ContractName::from(contract_name);
    let code_body = StacksString::from_string(&contract_content.to_string()).unwrap();

    let payload = TransactionSmartContract { name, code_body };

    serialize_sign_standard_single_sig_tx_anchor_mode(
        payload.into(),
        sender,
        nonce,
        tx_fee,
        TransactionAnchorMode::OffChainOnly,
    )
}

pub fn new_test_conf() -> Config {
    // secretKey: "b1cf9cee5083f421c84d7cb53be5edf2801c3c78d63d53917aee0bdc8bd160ee01",
    // publicKey: "03e2ed46873d0db820e8c6001aabc082d72b5b900b53b7a1b9714fe7bde3037b81",
    // stacksAddress: "ST2VHM28V9E5QCRD6C73215KAPSBKQGPWTEE5CMQT"
    let mut rng = rand::thread_rng();
    let mut buf = [0u8; 8];
    rng.fill_bytes(&mut buf);

    let mut conf = Config::default();
    conf.node.working_dir = format!(
        "/tmp/stacks-node-tests/integrations-neon/{}-{}",
        to_hex(&buf),
        get_epoch_time_secs()
    );
    conf.node.seed =
        hex_bytes("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
    conf.add_initial_balance(
        "ST2VHM28V9E5QCRD6C73215KAPSBKQGPWTEE5CMQT".to_string(),
        10000,
    );

    let rpc_port = u16::from_be_bytes(buf[0..2].try_into().unwrap()).saturating_add(1025) - 1; // use a non-privileged port between 1024 and 65534
    let p2p_port = u16::from_be_bytes(buf[2..4].try_into().unwrap()).saturating_add(1025) - 1; // use a non-privileged port between 1024 and 65534

    let localhost = "127.0.0.1";
    conf.node.rpc_bind = format!("{}:{}", localhost, rpc_port);
    conf.node.p2p_bind = format!("{}:{}", localhost, p2p_port);
    conf.node.data_url = format!("https://{}:{}", localhost, rpc_port);
    conf.node.p2p_address = format!("{}:{}", localhost, p2p_port);
    conf
}

pub fn to_addr(sk: &StacksPrivateKey) -> StacksAddress {
    StacksAddress::from_public_keys(
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![StacksPublicKey::from_private(sk)],
    )
    .unwrap()
}

pub fn make_stacks_transfer(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    recipient: &PrincipalData,
    amount: u64,
) -> Vec<u8> {
    let payload =
        TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));
    serialize_sign_standard_single_sig_tx(payload.into(), sender, nonce, tx_fee)
}

pub fn make_sponsored_stacks_transfer_on_testnet(
    sender: &StacksPrivateKey,
    payer: &StacksPrivateKey,
    sender_nonce: u64,
    payer_nonce: u64,
    tx_fee: u64,
    recipient: &PrincipalData,
    amount: u64,
) -> Vec<u8> {
    let payload =
        TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));
    serialize_sign_sponsored_sig_tx_anchor_mode_version(
        payload.into(),
        sender,
        payer,
        sender_nonce,
        payer_nonce,
        tx_fee,
        TransactionAnchorMode::OnChainOnly,
        TransactionVersion::Testnet,
    )
}

pub fn make_stacks_transfer_mblock_only(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    recipient: &PrincipalData,
    amount: u64,
) -> Vec<u8> {
    let payload =
        TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));
    serialize_sign_standard_single_sig_tx_anchor_mode(
        payload.into(),
        sender,
        nonce,
        tx_fee,
        TransactionAnchorMode::OffChainOnly,
    )
}

pub fn make_poison(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    header_1: StacksMicroblockHeader,
    header_2: StacksMicroblockHeader,
) -> Vec<u8> {
    let payload = TransactionPayload::PoisonMicroblock(header_1, header_2);
    serialize_sign_standard_single_sig_tx(payload.into(), sender, nonce, tx_fee)
}

pub fn make_coinbase(sender: &StacksPrivateKey, nonce: u64, tx_fee: u64) -> Vec<u8> {
    let payload = TransactionPayload::Coinbase(CoinbasePayload([0; 32]));
    serialize_sign_standard_single_sig_tx(payload.into(), sender, nonce, tx_fee)
}

pub fn make_contract_call(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    contract_addr: &StacksAddress,
    contract_name: &str,
    function_name: &str,
    function_args: &[Value],
) -> Vec<u8> {
    let contract_name = ContractName::from(contract_name);
    let function_name = ClarityName::from(function_name);

    let payload = TransactionContractCall {
        address: contract_addr.clone(),
        contract_name,
        function_name,
        function_args: function_args.iter().map(|x| x.clone()).collect(),
    };

    serialize_sign_standard_single_sig_tx(payload.into(), sender, nonce, tx_fee)
}

pub fn make_contract_call_mblock_only(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    contract_addr: &StacksAddress,
    contract_name: &str,
    function_name: &str,
    function_args: &[Value],
) -> Vec<u8> {
    let contract_name = ContractName::from(contract_name);
    let function_name = ClarityName::from(function_name);

    let payload = TransactionContractCall {
        address: contract_addr.clone(),
        contract_name,
        function_name,
        function_args: function_args.iter().map(|x| x.clone()).collect(),
    };

    serialize_sign_standard_single_sig_tx_anchor_mode(
        payload.into(),
        sender,
        nonce,
        tx_fee,
        TransactionAnchorMode::OffChainOnly,
    )
}

fn make_microblock(
    privk: &StacksPrivateKey,
    chainstate: &mut StacksChainState,
    burn_dbconn: &dyn BurnStateDB,
    consensus_hash: ConsensusHash,
    block: StacksBlock,
    txs: Vec<StacksTransaction>,
) -> StacksMicroblock {
    let mut block_bytes = vec![];
    block.consensus_serialize(&mut block_bytes).unwrap();

    let mut microblock_builder = StacksMicroblockBuilder::new(
        block.block_hash(),
        consensus_hash.clone(),
        chainstate,
        burn_dbconn,
        BlockBuilderSettings::max_value(),
    )
    .unwrap();
    let mempool_txs: Vec<_> = txs
        .into_iter()
        .map(|tx| {
            // TODO: better fee estimation
            let mut tx_bytes = vec![];
            tx.consensus_serialize(&mut tx_bytes).unwrap();
            (tx, tx_bytes.len() as u64)
        })
        .collect();

    // NOTE: we intentionally do not check the block's microblock pubkey hash against the private
    // key, because we may need to test that microblocks get rejected due to bad signatures.
    let microblock = microblock_builder
        .mine_next_microblock_from_txs(mempool_txs, privk)
        .unwrap();
    microblock
}

#[test]
fn should_succeed_mining_valid_txs() {
    let mut conf = new_test_conf();

    conf.add_initial_balance(
        to_addr(
            &StacksPrivateKey::from_hex(
                "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            )
            .unwrap(),
        )
        .to_string(),
        10000,
    );
    conf.add_initial_balance(
        to_addr(
            &StacksPrivateKey::from_hex(
                "b1cf9cee5083f421c84d7cb53be5edf2801c3c78d63d53917aee0bdc8bd160ee01",
            )
            .unwrap(),
        )
        .to_string(),
        100000,
    );

    conf.miner.min_tx_fee = 0;

    let num_rounds = 6;
    let mut run_loop = RunLoop::new(conf.clone());

    // Use tenure's hook for submitting transactions
    run_loop.callbacks.on_new_tenure(|round, _burnchain_tip, chain_tip, tenure| {
        let header_hash = chain_tip.block.block_hash();
        let consensus_hash = chain_tip.metadata.consensus_hash;

        let mut chainstate_copy = tenure.open_chainstate();

        match round {
            1 => {
                // On round 1, publish the KV contract
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash, PUBLISH_CONTRACT.to_owned(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                ).unwrap();
            },
            2 => {
                // On round 2, publish a "get:foo" transaction
                // ./blockstack-cli --testnet contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 1 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store get-value -e \"foo\"
                let get_foo = "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000001000000000000000a0100b7ff8b6c20c427b4f4f09c1ad7e50027e2b076b2ddc0ab55e64ef5ea3771dd4763a79bc5a2b1a79b72ce03dd146ccf24b84942d675a815819a8b85aa8065dfaa030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265096765742d76616c7565000000010d00000003666f6f";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(get_foo).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            3 => {
                // On round 3, publish a "set:foo=bar" transaction
                // ./blockstack-cli --testnet contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 2 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store set-value -e \"foo\" -e \"bar\" 
                let set_foo_bar = "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000002000000000000000a010142a01caf6a32b367664869182f0ebc174122a5a980937ba259d44cc3ebd280e769a53dd3913c8006ead680a6e1c98099fcd509ce94b0a4e90d9f4603b101922d030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265097365742d76616c7565000000020d00000003666f6f0d00000003626172";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(set_foo_bar).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            4 => {
                // On round 4, publish a "get:foo" transaction
                // ./blockstack-cli --testnet contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 3 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store get-value -e \"foo\"
                let get_foo = "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000003000000000000000a010046c2c1c345231443fef9a1f64fccfef3e1deacc342b2ab5f97612bb3742aa799038b20aea456789aca6b883e52f84a31adfee0bc2079b740464877af8f2f87d2030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265096765742d76616c7565000000010d00000003666f6f";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(get_foo).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            5 => {
                // On round 5, publish a stacks transaction
                // ./blockstack-cli --testnet token-transfer b1cf9cee5083f421c84d7cb53be5edf2801c3c78d63d53917aee0bdc8bd160ee01 10 0 ST195Q2HPXY576N4CT2A0R94D7DRYSX54A5X3YZTH 1000
                let transfer_1000_stx = "80800000000400b71a091b4b8b7661a661c620966ab6573bc2dcd30000000000000000000000000000000a0000393810832bacd44cfc4024980876135de6b95429bdb610d5ce96a92c9ee9bfd81ec77ea0f1748c8515fc9a1589e51d8b92bf028e3e84ade1249682c05271d5b803020000000000051a525b8a36ef8a73548cd0940c248d3b71ecf4a45100000000000003e800000000000000000000000000000000000000000000000000000000000000000000";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(transfer_1000_stx).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            _ => {}
        };
        return
    });

    // Use block's hook for asserting expectations
    run_loop.callbacks.on_new_stacks_chain_state(
        |round, _burnchain_tip, chain_tip, _chain_state, _burn_dbconn| {
            match round {
                0 => {
                    // Inspecting the chain at round 0.
                    // - Chain length should be 1.
                    assert!(chain_tip.metadata.block_height == 1);

                    // Block #1 should only have 0 txs
                    assert!(chain_tip.block.txs.len() == 1);

                    // 1 lockup event should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    println!("{:?}", events);
                    assert_eq!(events.len(), 1);
                }
                1 => {
                    // Inspecting the chain at round 1.
                    // - Chain length should be 2.
                    assert!(chain_tip.metadata.block_height == 2);

                    // Block #2 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the smart contract published
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::SmartContract(_) => true,
                        _ => false,
                    });

                    // 0 event should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    assert!(events.len() == 0);
                }
                2 => {
                    // Inspecting the chain at round 2.
                    // - Chain length should be 3.
                    assert!(chain_tip.metadata.block_height == 3);

                    // Block #3 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the get-value contract-call
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::ContractCall(_) => true,
                        _ => false,
                    });

                    // 2 lockup events should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    assert_eq!(events.len(), 2);
                }
                3 => {
                    // Inspecting the chain at round 3.
                    // - Chain length should be 4.
                    assert!(chain_tip.metadata.block_height == 4);

                    // Block #4 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the set-value contract-call
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::ContractCall(_) => true,
                        _ => false,
                    });

                    // 2 lockup events + 1 contract event should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    assert_eq!(events.len(), 3);
                    assert!(match &events.last().unwrap() {
                        StacksTransactionEvent::SmartContractEvent(data) => {
                            format!("{}", data.key.0)
                                == "STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A.store"
                                && data.key.1 == "print"
                                && format!("{}", data.value) == "\"Setting key foo\"".to_string()
                        }
                        _ => false,
                    });
                }
                4 => {
                    // Inspecting the chain at round 4.
                    // - Chain length should be 5.
                    assert!(chain_tip.metadata.block_height == 5);

                    // Block #5 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the get-value contract-call
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::ContractCall(_) => true,
                        _ => false,
                    });

                    // 1 event should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    assert!(events.len() == 1);
                    assert!(match &events[0] {
                        StacksTransactionEvent::SmartContractEvent(data) => {
                            format!("{}", data.key.0)
                                == "STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A.store"
                                && data.key.1 == "print"
                                && format!("{}", data.value) == "\"Getting key foo\"".to_string()
                        }
                        _ => false,
                    });
                }
                5 => {
                    // Inspecting the chain at round 5.
                    // - Chain length should be 6.
                    assert!(chain_tip.metadata.block_height == 6);

                    // Block #6 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the STX transfer
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);

                    assert!(match contract_tx.payload {
                        TransactionPayload::TokenTransfer(_, _, _) => true,
                        _ => false,
                    });

                    // 1 event should have been produced
                    let events: Vec<StacksTransactionEvent> = chain_tip
                        .receipts
                        .iter()
                        .flat_map(|a| a.events.clone())
                        .collect();
                    assert!(events.len() == 1);
                    assert!(match &events[0] {
                        StacksTransactionEvent::STXEvent(STXEventType::STXTransferEvent(event)) => {
                            format!("{}", event.recipient)
                                == "ST195Q2HPXY576N4CT2A0R94D7DRYSX54A5X3YZTH"
                                && format!("{}", event.sender)
                                    == "ST2VHM28V9E5QCRD6C73215KAPSBKQGPWTEE5CMQT"
                                && event.amount == 1000
                        }
                        _ => false,
                    });
                }
                _ => {}
            }
        },
    );
    run_loop.start(num_rounds).unwrap();
}

#[test]
#[ignore]
fn should_succeed_handling_malformed_and_valid_txs() {
    let mut conf = new_test_conf();
    let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
    conf.add_initial_balance(
        to_addr(
            &StacksPrivateKey::from_hex(
                "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            )
            .unwrap(),
        )
        .to_string(),
        10000,
    );
    conf.add_initial_balance(to_addr(&contract_sk).to_string(), 10000);

    let num_rounds = 4;
    let mut run_loop = RunLoop::new(conf);

    // Use tenure's hook for submitting transactions
    run_loop.callbacks.on_new_tenure(|round, _burnchain_tip, chain_tip, tenure| {
        let header_hash = chain_tip.block.block_hash();
        let consensus_hash = chain_tip.metadata.consensus_hash;
        let mut chainstate_copy = tenure.open_chainstate();

        match round {
            1 => {
                // On round 1, publish the KV contract
                let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
                let publish_contract = make_contract_publish(&contract_sk, 0, 10, "store", STORE_CONTRACT);
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,publish_contract,
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            2 => {
                // On round 2, publish a "get:foo" transaction (mainnet instead of testnet).
                // Will not be mined
                // ./blockstack-cli contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 1 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store get-value -e \"foo\"
                let get_foo = "0000000001040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000001000000000000000a0101ef2b00e7e55ee5cb7684d5313c7c49680c97e60cb29f0166798e6ffabd984a030cf0a7b919bcf5fa052efd5d9efd96b927213cb3af1cfb8d9c5a0be0fccda64d030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265096765742d76616c7565000000010d00000003666f6f";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(get_foo).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                                           ).unwrap();
            },
            3 => {
                // On round 3, publish a "set:foo=bar" transaction (chain-id not matching).
                // Will not be mined
                // ./blockstack-cli --testnet contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 1 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store set-value -e \"foo\" -e \"bar\"
                let set_foo_bar = "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000001000000000000000a010093f733efcebe2b239bb22e2e1ed25612547403af66b29282ed1f6fdfbbbf8f7f6ef107256d07947cbb72e165d723af99c447d6e25e7fbb6a92fd9a51c5ef7ee9030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265097365742d76616c7565000000020d00000003666f6f0d00000003626172";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(set_foo_bar).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                ).unwrap();
            },
            4 => {
                // On round 4, publish a "get:foo" transaction
                // ./blockstack-cli --testnet contract-call 043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3 10 1 STGT7GSMZG7EA0TS6MVSKT5JC1DCDFGZWJJZXN8A store get-value -e \"foo\"
                let get_foo = "8080000000040021a3c334fc0ee50359353799e8b2605ac6be1fe40000000000000001000000000000000a0100b7ff8b6c20c427b4f4f09c1ad7e50027e2b076b2ddc0ab55e64ef5ea3771dd4763a79bc5a2b1a79b72ce03dd146ccf24b84942d675a815819a8b85aa8065dfaa030200000000021a21a3c334fc0ee50359353799e8b2605ac6be1fe40573746f7265096765742d76616c7565000000010d00000003666f6f";
                tenure.mem_pool.submit_raw(&mut chainstate_copy, &consensus_hash, &header_hash,hex_bytes(get_foo).unwrap().to_vec(),
                                &ExecutionCost::max_value(),
                                &StacksEpochId::Epoch20,
                ).unwrap();
            },
            _ => {}
        };
        return
    });

    // Use block's hook for asserting expectations
    run_loop.callbacks.on_new_stacks_chain_state(
        |round, _burnchain_tip, chain_tip, _chain_state, _burn_dbconn| {
            match round {
                0 => {
                    // Inspecting the chain at round 0.
                    // - Chain length should be 1.
                    assert!(chain_tip.metadata.block_height == 1);

                    // Block #1 should only have 1 txs
                    assert!(chain_tip.block.txs.len() == 1);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });
                }
                1 => {
                    // Inspecting the chain at round 1.
                    // - Chain length should be 2.
                    assert!(chain_tip.metadata.block_height == 2);

                    // Block #2 should only have 2 txs
                    assert_eq!(chain_tip.block.txs.len(), 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the smart contract published
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::SmartContract(_) => true,
                        _ => false,
                    });
                }
                2 => {
                    // Inspecting the chain at round 2.
                    // - Chain length should be 3.
                    assert!(chain_tip.metadata.block_height == 3);

                    // Block #3 should only have 1 tx (the other being invalid)
                    assert!(chain_tip.block.txs.len() == 1);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });
                }
                3 => {
                    // Inspecting the chain at round 3.
                    // - Chain length should be 4.
                    assert!(chain_tip.metadata.block_height == 4);

                    // Block #4 should only have 1 tx (the other being invalid)
                    assert!(chain_tip.block.txs.len() == 1);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });
                }
                4 => {
                    // Inspecting the chain at round 4.
                    // - Chain length should be 5.
                    assert!(chain_tip.metadata.block_height == 5);

                    // Block #5 should only have 2 txs
                    assert!(chain_tip.block.txs.len() == 2);

                    // Transaction #1 should be the coinbase from the leader
                    let coinbase_tx = &chain_tip.block.txs[0];
                    assert!(coinbase_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match coinbase_tx.payload {
                        TransactionPayload::Coinbase(_) => true,
                        _ => false,
                    });

                    // Transaction #2 should be the contract-call
                    let contract_tx = &chain_tip.block.txs[1];
                    assert!(contract_tx.chain_id == CHAIN_ID_TESTNET);
                    assert!(match contract_tx.payload {
                        TransactionPayload::ContractCall(_) => true,
                        _ => false,
                    });
                }
                _ => {}
            }
        },
    );
    run_loop.start(num_rounds).unwrap();
}

#[test]
fn test_btc_to_sat() {
    let inputs = [
        "0.10000000",
        "0.00000010",
        "0.00000001",
        "1.00000001",
        "0.1",
        "0.00000000",
        "0.00001192",
    ];
    let expected_outputs: [u64; 7] = [10000000, 10, 1, 100000001, 10000000, 0, 1192];

    for (input, expected_output) in inputs.iter().zip(expected_outputs.iter()) {
        let output = ParsedUTXO::serialized_btc_to_sat(input).unwrap();
        assert_eq!(*expected_output, output);
    }
}

#[test]
fn test_btc_to_sat_errors() {
    assert!(ParsedUTXO::serialized_btc_to_sat("0.000000001").is_none());
    assert!(ParsedUTXO::serialized_btc_to_sat("1").is_none());
    assert!(ParsedUTXO::serialized_btc_to_sat("1e-8").is_none());
    assert!(ParsedUTXO::serialized_btc_to_sat("7.4e-7").is_none());
    assert!(ParsedUTXO::serialized_btc_to_sat("5.96e-6").is_none());
}
