/* tag::catalog[]
Title:: Create Subnet

Goal:: Ensure that a subnet can be created from unassigned nodes

Runbook::
. set up the NNS subnet and check that we have unassigned nodes
. submit a proposal for creating a subnet based on those unassigned nodes
. validate proposal execution by checking if the new subnet has been registered as expected
. validate that the new subnet is operational by installing and querying a universal canister

Success::
. subnet creation proposal is adopted and executed
. registry subnet list equals OldSubnetIDs ∪ { new_subnet_id }
. newly created subnet endpoint comes to life within 2 minutes
. universal canister can be installed onto the new subnet
. universal canister is responsive

end::catalog[] */

use std::collections::HashSet;
use std::iter::FromIterator;
use std::time::{Duration, Instant};

use crate::driver::ic::{InternetComputer, Subnet};
use crate::util::{assert_endpoints_reachability, get_unassinged_nodes_endpoints, EndpointsStatus};
use ic_base_types::NodeId;
use ic_fondue::ic_manager::IcHandle;
use ic_fondue::ic_manager::IcSubnet;
use slog::info;

use ic_registry_nns_data_provider::registry::RegistryCanister;
use ic_registry_subnet_type::SubnetType;
use ic_types::Height;

use crate::nns::{
    self, get_software_version, submit_create_application_subnet_proposal,
    vote_execute_proposal_assert_executed,
};
use crate::nns::{get_subnet_list_from_registry, NnsExt};

use crate::util::{
    assert_create_agent, block_on, get_random_nns_node_endpoint, runtime_from_url,
    UniversalCanister,
};

const NNS_SUBNET_SIZE: usize = 40;
const APP_SUBNET_SIZE: usize = 34; // f*3+1 with f=11
const NNS_PRE_MASTER: usize = 4;
const APP_PRE_MASTER: usize = 4;

// Small IC for correctness test pre-master
pub fn pre_master_config() -> InternetComputer {
    InternetComputer::new()
        .add_subnet(
            Subnet::fast(SubnetType::System, NNS_PRE_MASTER)
                .with_dkg_interval_length(Height::from(NNS_PRE_MASTER as u64 * 2)),
        )
        .with_unassigned_nodes(APP_PRE_MASTER as i32)
}

// IC with large subnets for a more resource-intensive test
pub fn hourly_config() -> InternetComputer {
    InternetComputer::new()
        .add_subnet(
            Subnet::fast(SubnetType::System, NNS_SUBNET_SIZE)
                .with_dkg_interval_length(Height::from(NNS_SUBNET_SIZE as u64 * 2)),
        )
        .with_unassigned_nodes(APP_SUBNET_SIZE as i32)
}

pub fn test(handle: IcHandle, ctx: &ic_fondue::pot::Context) {
    let mut rng = ctx.rng.clone();
    let unassigned_endpoints = get_unassinged_nodes_endpoints(&handle);
    info!(
        ctx.logger,
        "Checking readiness of all nodes after the IC setup ..."
    );
    block_on(async {
        // Check readiness of all assigned nodes.
        let nns_endpoints: Vec<_> = handle
            .as_permutation(&mut rng)
            .filter(|e| e.subnet.as_ref().map(|s| s.type_of) == Some(SubnetType::System))
            .collect::<Vec<_>>();
        assert_endpoints_reachability(nns_endpoints.as_slice(), EndpointsStatus::AllReachable)
            .await;
        // Check readiness of all unassigned nodes.
        for ep in unassigned_endpoints.iter() {
            ep.assert_ready_with_start(Instant::now(), ctx).await;
        }
    });
    info!(ctx.logger, "All nodes are ready, IC setup succeeded.");
    // [Phase I] Prepare NNS
    ctx.install_nns_canisters(&handle, true);
    let endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(endpoint.assert_ready(ctx));

    // get IDs of (1) all nodes (2) unassigned nodes
    let node_ids = ctx.initial_node_ids(&handle);
    let unassigned_nodes: Vec<NodeId> = unassigned_endpoints.iter().map(|ep| ep.node_id).collect();

    // check that (1) unassigned nodes are a subset of all the nodes and (2) there
    // is at least one unassigned node
    assert!(
        set(&unassigned_nodes).is_subset(&set(&node_ids)),
        "could not obtain unassigned nodes"
    );
    assert!(
        !unassigned_nodes.is_empty(),
        "there must be at least one unassigned node for creating a subnet"
    );

    // [Phase II] Execute and validate the testnet change

    let client = RegistryCanister::new_with_query_timeout(
        vec![endpoint.url.clone()],
        Duration::from_secs(10),
    );

    let new_subnet: IcSubnet = block_on(async move {
        // get original subnet ids
        let original_subnets = get_subnet_list_from_registry(&client).await;
        assert!(!original_subnets.is_empty(), "registry contains no subnets");
        info!(ctx.logger, "original subnets: {:?}", original_subnets);

        // get current replica version and Governance canister
        let version = get_software_version(endpoint)
            .await
            .expect("could not obtain replica software version");
        let nns = runtime_from_url(endpoint.url.clone());
        let governance = nns::get_governance_canister(&nns);

        let proposal_id =
            submit_create_application_subnet_proposal(&governance, unassigned_nodes, version).await;

        vote_execute_proposal_assert_executed(&governance, proposal_id).await;

        // Check that the registry indeed contains the data
        let final_subnets = get_subnet_list_from_registry(&client).await;
        info!(ctx.logger, "final subnets: {:?}", final_subnets);

        // check that there is exactly one added subnet
        assert_eq!(
            original_subnets.len() + 1,
            final_subnets.len(),
            "final number of subnets should be one above number of original subnets"
        );
        let original_subnet_set = set(&original_subnets);
        let final_subnet_set = set(&final_subnets);
        assert!(
            original_subnet_set.is_subset(&final_subnet_set),
            "final number of subnets should be a superset of the set of original subnets"
        );

        // Return the newly created subnet
        let new_subnet_id = original_subnet_set
            .symmetric_difference(&final_subnet_set)
            .collect::<HashSet<_>>()
            .iter()
            .next()
            .unwrap()
            .to_owned()
            .to_owned();
        IcSubnet {
            id: new_subnet_id,
            type_of: SubnetType::Application,
        }
    });

    info!(
        ctx.logger,
        "created application subnet with ID {}", new_subnet.id
    );

    let unassigned_endpoint = unassigned_endpoints.into_iter().next().unwrap();

    // [Phase III] install a canister onto that subnet and check that it is
    // operational
    block_on(async move {
        let newly_assigned_endpoint = unassigned_endpoint.recreate_with_subnet(new_subnet);

        newly_assigned_endpoint.assert_ready(ctx).await;

        let agent = assert_create_agent(newly_assigned_endpoint.url.as_str()).await;
        info!(
            ctx.logger,
            "successfully created agent for endpoint of an originally unassigned node"
        );

        let universal_canister = UniversalCanister::new(&agent).await;
        info!(
            ctx.logger,
            "successfully created a universal canister instance"
        );

        const UPDATE_MSG_1: &[u8] =
            b"This beautiful prose should be persisted for future generations";

        universal_canister.store_to_stable(0, UPDATE_MSG_1).await;
        info!(
            ctx.logger,
            "successfully saved message in the universal canister"
        );

        assert_eq!(
            universal_canister
                .try_read_stable(0, UPDATE_MSG_1.len() as u32)
                .await,
            UPDATE_MSG_1.to_vec(),
            "could not validate that subnet is healthy: universal canister is broken"
        );
    });

    info!(
        ctx.logger,
        "Successfully created an app subnet of size {} from an NNS subnet of size {}",
        APP_SUBNET_SIZE,
        NNS_SUBNET_SIZE
    );
}

fn set<H: Clone + std::cmp::Eq + std::hash::Hash>(data: &[H]) -> HashSet<H> {
    HashSet::from_iter(data.iter().cloned())
}
