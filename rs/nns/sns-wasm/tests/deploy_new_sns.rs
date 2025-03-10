use candid::{Decode, Encode};
use canister_test::{Canister, Project, Runtime, Wasm};
use dfn_candid::candid_one;
use ic_base_types::{CanisterId, PrincipalId, SubnetId};
use ic_crypto_sha::Sha256;
use ic_ic00_types::CanisterStatusResultV2;
use ic_interfaces::registry::RegistryClient;
use ic_nns_constants::{
    ROOT_CANISTER_ID, SNS_WASM_CANISTER_ID, SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET,
};
use ic_nns_test_utils::itest_helpers::{
    local_test_on_nns_subnet, set_up_universal_canister,
    try_call_with_cycles_via_universal_canister, NnsCanisters,
};
use ic_nns_test_utils::sns_wasm;
use ic_nns_test_utils::state_test_helpers;
use ic_protobuf::registry::subnet::v1::SubnetListRecord;
use ic_registry_keys::make_subnet_list_record_key;
use ic_sns_init::pb::v1::SnsInitPayload;
use ic_sns_root::{
    CanisterStatusType::Running, GetSnsCanistersSummaryRequest, GetSnsCanistersSummaryResponse,
};
use ic_sns_swap::pb::v1::GetCanisterStatusRequest;
use ic_sns_wasm::pb::v1::{
    AddWasmRequest, DeployNewSnsRequest, DeployNewSnsResponse, SnsCanisterIds, SnsCanisterType,
    SnsWasm, SnsWasmError,
};
use ic_test_utilities::types::ids::canister_test_id;
use ic_test_utilities::universal_canister::UNIVERSAL_CANISTER_WASM;
use ic_types::Cycles;
use registry_canister::mutations::common::decode_registry_value;
use std::convert::TryFrom;
pub mod common;
use common::set_up_state_machine_with_nns;
use ic_nns_test_utils::common::NnsInitPayloadsBuilder;

#[test]
fn test_canisters_are_created_and_installed() {
    // Keeping a test on ReplicaTests for performance comparison
    local_test_on_nns_subnet(|runtime| async move {
        let fake_registry_client = match runtime {
            Runtime::Remote(_) => {
                panic!("Cannot run this test on Runtime::Remote at this time");
            }
            Runtime::Local(ref r) => r.registry_client.clone(),
        };

        let subnet_list_record = decode_registry_value::<SubnetListRecord>(
            fake_registry_client
                .get_value(
                    &make_subnet_list_record_key(),
                    fake_registry_client.get_latest_version(),
                )
                .unwrap()
                .unwrap(),
        );
        let system_subnet_id = SubnetId::new(
            PrincipalId::try_from(subnet_list_record.subnets.get(0).unwrap()).unwrap(),
        );

        let nns_init_payload = NnsInitPayloadsBuilder::new()
            .with_initial_invariant_compliant_mutations()
            .with_test_neurons()
            .with_sns_dedicated_subnets(vec![system_subnet_id])
            .build();
        let nns_canisters = NnsCanisters::set_up(&runtime, nns_init_payload).await;

        let sns_wasm = &nns_canisters.sns_wasms;

        let root_wasm =
            Project::cargo_bin_maybe_use_path_relative_to_rs("sns/root", "sns-root-canister", &[]);
        let root_hash = Sha256::hash(&root_wasm.clone().bytes()).to_vec();
        let request = AddWasmRequest {
            wasm: Some(SnsWasm {
                wasm: root_wasm.clone().bytes(),
                canister_type: SnsCanisterType::Root.into(),
            }),
            hash: root_hash.clone(),
        };
        nns_canisters.add_wasm(request).await;

        let governance_wasm = Project::cargo_bin_maybe_use_path_relative_to_rs(
            "sns/governance",
            "sns-governance-canister",
            &[],
        );
        let governance_hash = Sha256::hash(&governance_wasm.clone().bytes()).to_vec();
        let request = AddWasmRequest {
            wasm: Some(SnsWasm {
                wasm: governance_wasm.clone().bytes(),
                canister_type: SnsCanisterType::Governance.into(),
            }),
            hash: governance_hash.clone(),
        };
        nns_canisters.add_wasm(request).await;

        let ledger_wasm = Project::cargo_bin_maybe_use_path_relative_to_rs(
            "rosetta-api/icrc1/ledger",
            "ic-icrc1-ledger",
            &[],
        );
        let ledger_hash = Sha256::hash(&ledger_wasm.clone().bytes()).to_vec();
        let request = AddWasmRequest {
            wasm: Some(SnsWasm {
                wasm: ledger_wasm.clone().bytes(),
                canister_type: SnsCanisterType::Ledger.into(),
            }),
            hash: ledger_hash.clone(),
        };
        nns_canisters.add_wasm(request).await;

        let swap_wasm =
            Project::cargo_bin_maybe_use_path_relative_to_rs("sns/swap", "sns-swap-canister", &[]);
        let swap_hash = Sha256::hash(&swap_wasm.clone().bytes()).to_vec();
        let request = AddWasmRequest {
            wasm: Some(SnsWasm {
                wasm: swap_wasm.clone().bytes(),
                canister_type: SnsCanisterType::Swap.into(),
            }),
            hash: swap_hash.clone(),
        };

        nns_canisters.add_wasm(request).await;

        let wallet_with_unlimited_cycles = set_up_universal_canister(&runtime).await;

        let result = try_call_with_cycles_via_universal_canister(
            &wallet_with_unlimited_cycles,
            sns_wasm,
            "deploy_new_sns",
            Encode!(&DeployNewSnsRequest {
                sns_init_payload: Some(SnsInitPayload::with_valid_values_for_testing())
            })
            .unwrap(),
            50_000_000_000_000,
        )
        .await
        .unwrap();

        let response = Decode!(&result, DeployNewSnsResponse).unwrap();

        // SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 1 is the ID of the wallet canister
        let root_canister_id = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 2);
        let governance_canister_id = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 3);
        let ledger_canister_id = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 4);
        let swap_canister_id = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 5);

        assert_eq!(
            response,
            DeployNewSnsResponse {
                subnet_id: Some(system_subnet_id.get()),
                canisters: Some(SnsCanisterIds {
                    governance: Some(governance_canister_id.get()),
                    root: Some(root_canister_id.get()),
                    ledger: Some(ledger_canister_id.get()),
                    swap: Some(swap_canister_id.get())
                }),
                error: None
            }
        );

        let canisters_returned = response.canisters.unwrap();
        let root_canister_principal = canisters_returned.root.unwrap();
        let swap_canister_principal = canisters_returned.swap.unwrap();

        let mut root_canister =
            Canister::new(&runtime, CanisterId::new(root_canister_principal).unwrap());
        root_canister.set_wasm(root_wasm.bytes());

        let response: GetSnsCanistersSummaryResponse = root_canister
            .update_(
                "get_sns_canisters_summary",
                candid_one,
                GetSnsCanistersSummaryRequest {
                    update_canister_list: None,
                },
            )
            .await
            .unwrap();

        // We know from a successful response that the init_payload is in fact sent correctly
        // through CanisterApiImpl::install_wasm, since governance has to know root canister_id
        // in order to respond to root's request for its own status from governance
        // more detailed coverage of the initialization parameters is done through unit tests

        // Assert that the canisters are installed in the same configuration that our response
        // told us above and controllers and installed wasms are correct
        let root_canister_summary = response.root_canister_summary();
        assert_eq!(root_canister_summary.canister_id(), root_canister_id.get());
        assert_eq!(root_canister_summary.status().status(), Running);
        assert_eq!(
            root_canister_summary.status().controller(),
            governance_canister_id.get()
        );
        assert_eq!(
            root_canister_summary.status().module_hash().unwrap(),
            root_hash
        );

        let governance_canister_summary = response.governance_canister_summary();
        assert_eq!(
            governance_canister_summary.canister_id(),
            governance_canister_id.get()
        );
        assert_eq!(governance_canister_summary.status().status(), Running);
        assert_eq!(
            governance_canister_summary.status().controller(),
            root_canister_id.get()
        );
        assert_eq!(
            governance_canister_summary.status().module_hash().unwrap(),
            governance_hash
        );

        let ledger_canister_summary = response.ledger_canister_summary();
        assert_eq!(
            ledger_canister_summary.canister_id(),
            ledger_canister_id.get()
        );
        assert_eq!(ledger_canister_summary.status().status(), Running);
        assert_eq!(
            ledger_canister_summary.status().controller(),
            root_canister_id.get()
        );
        assert_eq!(
            ledger_canister_summary.status().module_hash().unwrap(),
            ledger_hash
        );

        let mut swap_canister =
            Canister::new(&runtime, CanisterId::new(swap_canister_principal).unwrap());
        swap_canister.set_wasm(swap_wasm.bytes());

        // Check Swap status
        let response: CanisterStatusResultV2 = swap_canister
            .update_(
                "get_canister_status",
                candid_one,
                GetCanisterStatusRequest {},
            )
            .await
            .unwrap();

        assert_eq!(
            response.controllers(),
            vec![ROOT_CANISTER_ID.get(), swap_canister_id.get()]
        );
        Ok(())
    });
}

/// There are not many tests we can deterministically create at this level
/// to simulate failure without creating more sophisticated test harnesses that let us
/// simulate failures executing basic IC00 operations
#[test]
fn test_deploy_cleanup_on_wasm_install_failure() {
    let machine = set_up_state_machine_with_nns();

    // Enough cycles one SNS deploy
    let wallet_canister = state_test_helpers::set_up_universal_canister(
        &machine,
        Some(Cycles::new(50_000_000_000_000)),
    );

    sns_wasm::add_real_wasms_to_sns_wasms(&machine);
    // we add a wasm that will fail with the given payload on installation
    let bad_wasm = SnsWasm {
        wasm: Wasm::from_bytes(UNIVERSAL_CANISTER_WASM).bytes(),
        canister_type: SnsCanisterType::Governance.into(),
    };
    let bad_wasm_hash = bad_wasm.sha256_hash();
    sns_wasm::add_wasm_via_proposal(&machine, bad_wasm, &bad_wasm_hash);

    let response = sns_wasm::deploy_new_sns(
        &machine,
        wallet_canister,
        SNS_WASM_CANISTER_ID,
        SnsInitPayload::with_valid_values_for_testing(),
        50_000_000_000_000,
    );

    assert_eq!(
        response,
        DeployNewSnsResponse {
            subnet_id: None,
            canisters: None,
            // Because of the invalid WASM above (i.e. universal canister) which does not understand
            // the governance init payload, this fails.
            error: Some(SnsWasmError {
                message: "Error installing Governance WASM: Failed to install WASM on canister qsgjb-riaaa-aaaaa-aaaga-cai: \
                error code 5: Canister qsgjb-riaaa-aaaaa-aaaga-cai trapped explicitly: \
                unknown op 68"
                    .to_string()
            })
        }
    );

    // 2_000_000_000 cycles are burned creating the canisters before the failure
    assert_eq!(machine.cycle_balance(wallet_canister), 48_000_000_000_000);

    // No canisters should exist above SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 1 (+1 for the wallet
    // canister) because we deleted those canisters
    assert!(!machine.canister_exists(canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 2)));
    assert!(!machine.canister_exists(canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 3)));
    assert!(!machine.canister_exists(canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 4)));
    assert!(!machine.canister_exists(canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 5)));
}

#[test]
fn test_deploy_adds_cycles_to_target_canisters() {
    let machine = set_up_state_machine_with_nns();

    // Enough cycles one SNS deploy
    let wallet_canister = state_test_helpers::set_up_universal_canister(
        &machine,
        Some(Cycles::new(50_000_000_000_000)),
    );

    sns_wasm::add_dummy_wasms_to_sns_wasms(&machine);
    // we add a wasm that will fail with the given payload on installation

    let response = sns_wasm::deploy_new_sns(
        &machine,
        wallet_canister,
        SNS_WASM_CANISTER_ID,
        SnsInitPayload::with_valid_values_for_testing(),
        50_000_000_000_000,
    );

    // SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 1 is the ID of the wallet canister
    let root = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 2);
    let governance = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 3);
    let ledger = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 4);
    let swap = canister_test_id(SNS_WASM_CANISTER_INDEX_IN_NNS_SUBNET + 5);

    assert_eq!(
        response,
        DeployNewSnsResponse {
            subnet_id: Some(machine.get_subnet_ids()[0].get()),
            canisters: Some(SnsCanisterIds {
                root: Some(*root.get_ref()),
                ledger: Some(*ledger.get_ref()),
                governance: Some(*governance.get_ref()),
                swap: Some(*swap.get_ref())
            }),
            error: None
        }
    );

    // All cycles should have been used and none refunded.
    assert_eq!(machine.cycle_balance(wallet_canister), 0);

    for canister_id in &[root, governance, ledger, swap] {
        assert!(machine.canister_exists(*canister_id));
        assert_eq!(machine.cycle_balance(*canister_id), 12_500_000_000_000)
    }
}
