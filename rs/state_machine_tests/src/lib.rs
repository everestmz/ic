use ic_config::subnet_config::{SubnetConfig, SubnetConfigs};
use ic_crypto_internal_seed::Seed;
use ic_crypto_internal_threshold_sig_bls12381::api::{
    combine_signatures, combined_public_key, keygen, sign_message,
};
use ic_crypto_internal_threshold_sig_bls12381::types::SecretKeyBytes;
use ic_crypto_internal_types::sign::threshold_sig::public_key::CspThresholdSigPublicKey;
use ic_crypto_tree_hash::{flatmap, Label, LabeledTree, LabeledTree::SubTree};
use ic_cycles_account_manager::CyclesAccountManager;
pub use ic_error_types::{ErrorCode, UserError};
use ic_execution_environment::ExecutionServices;
use ic_ic00_types::{self as ic00, CanisterIdRecord, InstallCodeArgs, Method, Payload};
pub use ic_ic00_types::{CanisterInstallMode, CanisterSettingsArgs};
use ic_interfaces::{
    certification::{Verifier, VerifierError},
    crypto::Signable,
    execution_environment::{IngressHistoryReader, QueryHandler},
    messaging::MessageRouting,
    registry::RegistryClient,
    validation::ValidationResult,
};
use ic_interfaces_state_manager::{CertificationScope, StateHashError, StateManager, StateReader};
use ic_logger::ReplicaLogger;
use ic_messaging::MessageRoutingImpl;
use ic_metrics::MetricsRegistry;
use ic_protobuf::registry::{
    node::v1::{ConnectionEndpoint, NodeRecord},
    provisional_whitelist::v1::ProvisionalWhitelist as PbProvisionalWhitelist,
    routing_table::v1::CanisterMigrations as PbCanisterMigrations,
    routing_table::v1::RoutingTable as PbRoutingTable,
};
use ic_protobuf::types::v1::PrincipalId as PrincipalIdIdProto;
use ic_protobuf::types::v1::SubnetId as SubnetIdProto;
use ic_registry_client_fake::FakeRegistryClient;
use ic_registry_client_helpers::subnet::SubnetListRegistry;
use ic_registry_keys::{
    make_canister_migrations_record_key, make_node_record_key,
    make_provisional_whitelist_record_key, make_routing_table_record_key, ROOT_SUBNET_ID_KEY,
};
use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
use ic_registry_provisional_whitelist::ProvisionalWhitelist;
use ic_registry_routing_table::{
    routing_table_insert_subnet, CanisterIdRange, CanisterIdRanges, RoutingTable,
};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::page_map::Buffer;
use ic_replicated_state::{
    canister_state::{NumWasmPages, WASM_PAGE_SIZE_IN_BYTES},
    Memory, PageMap, ReplicatedState,
};
use ic_state_manager::StateManagerImpl;
use ic_test_utilities_metrics::fetch_histogram_stats;
use ic_test_utilities_registry::{
    add_subnet_record, insert_initial_dkg_transcript, SubnetRecordBuilder,
};
use ic_types::consensus::certification::CertificationContent;
use ic_types::crypto::threshold_sig::ni_dkg::{NiDkgId, NiDkgTag, NiDkgTargetSubnet};
pub use ic_types::crypto::threshold_sig::ThresholdSigPublicKey;
use ic_types::crypto::{CombinedThresholdSig, CombinedThresholdSigOf, Signed};
use ic_types::messages::Certificate;
use ic_types::signature::ThresholdSignature;
use ic_types::{
    batch::{Batch, BatchPayload, IngressPayload},
    consensus::certification::Certification,
    messages::{
        Blob, HttpCallContent, HttpCanisterUpdate, HttpRequestEnvelope, SignedIngress, UserQuery,
    },
    time::current_time_and_expiry_time,
    CryptoHashOfPartialState, Height, NodeId, NumberOfNodes, Randomness, RegistryVersion,
};
pub use ic_types::{
    ingress::{IngressState, IngressStatus, WasmResult},
    messages::MessageId,
    time::Time,
    CanisterId, CryptoHashOfState, Cycles, PrincipalId, SubnetId, UserId,
};
use serde::Serialize;
pub use slog::Level;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::string::ToString;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{collections::BTreeMap, convert::TryFrom};
use tempfile::TempDir;
use tokio::runtime::Runtime;

struct FakeVerifier;
impl Verifier for FakeVerifier {
    fn validate(
        &self,
        _: SubnetId,
        _: &Certification,
        _: RegistryVersion,
    ) -> ValidationResult<VerifierError> {
        Ok(())
    }
}

const GENESIS: Time = Time::from_nanos_since_unix_epoch(1_620_328_630_000_000_000);

/// Constructs the initial version of the registry containing a subnet with the
/// specified SUBNET_ID, with the node with the specified NODE_ID assigned to
/// it.
fn make_single_node_registry(
    subnet_id: SubnetId,
    subnet_type: SubnetType,
    node_id: NodeId,
) -> (Arc<ProtoRegistryDataProvider>, Arc<FakeRegistryClient>) {
    let registry_version = RegistryVersion::from(1);
    let data_provider = Arc::new(ProtoRegistryDataProvider::new());

    let root_subnet_id_proto = SubnetIdProto {
        principal_id: Some(PrincipalIdIdProto {
            raw: subnet_id.get_ref().to_vec(),
        }),
    };
    data_provider
        .add(
            ROOT_SUBNET_ID_KEY,
            registry_version,
            Some(root_subnet_id_proto),
        )
        .unwrap();

    let mut routing_table = RoutingTable::new();
    routing_table_insert_subnet(&mut routing_table, subnet_id).unwrap();
    let pb_routing_table = PbRoutingTable::from(routing_table);
    data_provider
        .add(
            &make_routing_table_record_key(),
            registry_version,
            Some(pb_routing_table),
        )
        .unwrap();
    let pb_whitelist = PbProvisionalWhitelist::from(ProvisionalWhitelist::All);
    data_provider
        .add(
            &make_provisional_whitelist_record_key(),
            registry_version,
            Some(pb_whitelist),
        )
        .unwrap();
    let node_record = NodeRecord {
        node_operator_id: vec![0],
        xnet: None,
        http: Some(ConnectionEndpoint {
            ip_addr: "2a00:fb01:400:42:5000:22ff:fe5e:e3c4".into(),
            port: 1234,
            protocol: 0,
        }),
        p2p_flow_endpoints: vec![],
        prometheus_metrics_http: None,
        public_api: vec![],
        private_api: vec![],
        prometheus_metrics: vec![],
        xnet_api: vec![],
    };
    data_provider
        .add(
            &make_node_record_key(node_id),
            registry_version,
            Some(node_record),
        )
        .unwrap();

    // Set subnetwork list(needed for filling network_topology.nns_subnet_id)
    let record = SubnetRecordBuilder::from(&[node_id])
        .with_subnet_type(subnet_type)
        .build();

    insert_initial_dkg_transcript(registry_version.get(), subnet_id, &record, &data_provider);
    add_subnet_record(&data_provider, registry_version.get(), subnet_id, record);

    let registry_client = Arc::new(FakeRegistryClient::new(Arc::clone(&data_provider) as _));
    registry_client.update_to_latest_version();
    (data_provider, registry_client)
}

/// Convert an object into CBOR binary.
fn into_cbor<R: Serialize>(r: &R) -> Vec<u8> {
    let mut ser = serde_cbor::Serializer::new(Vec::new());
    ser.self_describe().expect("Could not write magic tag.");
    r.serialize(&mut ser).expect("Serialization failed.");
    ser.into_inner()
}

/// Represents a replicated state machine detached from the network layer that
/// can be used to test this part of the stack in isolation.
pub struct StateMachine {
    subnet_id: SubnetId,
    public_key: ThresholdSigPublicKey,
    secret_key: SecretKeyBytes,
    registry_data_provider: Arc<ProtoRegistryDataProvider>,
    registry_client: Arc<FakeRegistryClient>,
    state_manager: Arc<StateManagerImpl>,
    message_routing: MessageRoutingImpl,
    metrics_registry: MetricsRegistry,
    ingress_history_reader: Box<dyn IngressHistoryReader>,
    query_handler: Arc<dyn QueryHandler<State = ReplicatedState>>,
    _runtime: Runtime,
    state_dir: TempDir,
    checkpoints_enabled: std::cell::Cell<bool>,
    nonce: std::cell::Cell<u64>,
    time: std::cell::Cell<Time>,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for StateMachine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateMachine")
            .field("state_dir", &self.state_dir.path().display())
            .field("nonce", &self.nonce.get())
            .finish()
    }
}

impl StateMachine {
    /// Constructs a new environment that uses a temporary directory for storing
    /// states.
    pub fn new() -> Self {
        Self::setup_from_dir(
            TempDir::new().expect("failed to create a temporary directory"),
            0,
            GENESIS,
            None,
            false,
        )
    }

    /// Constructs a new environment with the specified configuration.
    pub fn new_with_config(config: SubnetConfig) -> Self {
        Self::setup_from_dir(
            TempDir::new().expect("failed to create a temporary directory"),
            0,
            GENESIS,
            Some(config),
            false,
        )
    }

    /// Constructs and initializes a new state machine that uses the specified
    /// directory for storing states.
    fn setup_from_dir(
        state_dir: TempDir,
        nonce: u64,
        time: Time,
        subnet_config: Option<SubnetConfig>,
        checkpoints_enabled: bool,
    ) -> Self {
        use slog::Drain;

        let log_level = std::env::var("RUST_LOG")
            .map(|level| Level::from_str(&level).unwrap())
            .unwrap_or_else(|_| Level::Warning);

        let decorator = slog_term::PlainSyncDecorator::new(slog_term::TestStdoutWriter);
        let drain = slog_term::FullFormat::new(decorator)
            .build()
            .filter_level(log_level)
            .fuse();
        let logger = slog::Logger::root(drain, slog::o!());
        let replica_logger: ReplicaLogger = logger.into();

        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(1));
        let node_id = NodeId::from(PrincipalId::new_node_test_id(1));
        let metrics_registry = MetricsRegistry::new();
        let subnet_type = SubnetType::System;
        let subnet_config = match subnet_config {
            Some(subnet_config) => subnet_config,
            None => SubnetConfigs::default().own_subnet_config(subnet_type),
        };

        let (registry_data_provider, registry_client) =
            make_single_node_registry(subnet_id, subnet_type, node_id);

        let sm_config = ic_config::state_manager::Config::new(state_dir.path().to_path_buf());
        let hypervisor_config = ic_config::execution_environment::Config {
            canister_sandboxing_flag: ic_config::flag_status::FlagStatus::Disabled,
            ..Default::default()
        };

        let cycles_account_manager = Arc::new(CyclesAccountManager::new(
            subnet_config.scheduler_config.max_instructions_per_message,
            subnet_type,
            subnet_id,
            subnet_config.cycles_account_manager_config,
        ));
        let state_manager = Arc::new(StateManagerImpl::new(
            Arc::new(FakeVerifier),
            subnet_id,
            subnet_type,
            replica_logger.clone(),
            &metrics_registry,
            &sm_config,
            None,
            ic_types::malicious_flags::MaliciousFlags::default(),
        ));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to create a tokio runtime");

        // NOTE: constructing execution services requires tokio context.
        //
        // We could have required the client to use [tokio::test] for state
        // machine tests, but this is error prone and leads to poor dev
        // experience.
        //
        // The API state machine provides is blocking anyway.
        let execution_services = runtime.block_on(async {
            ExecutionServices::setup_execution(
                replica_logger.clone(),
                &metrics_registry,
                subnet_id,
                subnet_type,
                subnet_config.scheduler_config,
                hypervisor_config.clone(),
                Arc::clone(&cycles_account_manager),
                Arc::clone(&state_manager) as Arc<_>,
            )
        });

        let message_routing = MessageRoutingImpl::new(
            Arc::clone(&state_manager) as _,
            Arc::clone(&state_manager) as _,
            Arc::clone(&execution_services.ingress_history_writer) as _,
            execution_services.scheduler,
            hypervisor_config,
            cycles_account_manager,
            subnet_id,
            &metrics_registry,
            replica_logger,
            Arc::clone(&registry_client) as _,
        );

        // fixed seed to keep tests reproducible
        let seed: [u8; 32] = [
            3, 5, 31, 46, 53, 66, 100, 101, 109, 121, 126, 129, 133, 152, 163, 165, 167, 186, 198,
            203, 206, 208, 211, 216, 229, 232, 233, 236, 242, 244, 246, 250,
        ];

        let (public_coefficients, secret_key_bytes) =
            keygen(Seed::from_bytes(&seed), NumberOfNodes::new(1), &[true; 1]).unwrap();
        let public_key = ThresholdSigPublicKey::from(CspThresholdSigPublicKey::from(
            combined_public_key(&public_coefficients).unwrap(),
        ));

        Self {
            subnet_id,
            secret_key: secret_key_bytes.get(0).unwrap().unwrap(),
            public_key,
            registry_data_provider,
            registry_client,
            state_manager,
            ingress_history_reader: execution_services.ingress_history_reader,
            message_routing,
            metrics_registry,
            query_handler: execution_services.sync_query_handler,
            _runtime: runtime,
            state_dir,
            // Note: state machine tests are commonly used for testing
            // canisters, such tests usually don't rely on any persistence.
            checkpoints_enabled: std::cell::Cell::new(checkpoints_enabled),
            nonce: std::cell::Cell::new(nonce),
            time: std::cell::Cell::new(time),
        }
    }

    /// Emulates a node restart, including checkpoint recovery.
    pub fn restart_node(self) -> Self {
        Self::setup_from_dir(
            self.state_dir,
            self.nonce.get(),
            self.time.get(),
            None,
            self.checkpoints_enabled.get(),
        )
    }

    /// Same as [restart_node], but the subnet will have the specified `config`
    /// after the restart.
    pub fn restart_node_with_config(self, config: SubnetConfig) -> Self {
        Self::setup_from_dir(
            self.state_dir,
            self.nonce.get(),
            self.time.get(),
            Some(config),
            self.checkpoints_enabled.get(),
        )
    }

    /// If the argument is true, the state machine will create an on-disk
    /// checkpoint for each new state it creates.
    ///
    /// You have to call this function with `true` before you make any changes
    /// to the state machine if you want to use [restart_node] and
    /// [await_state_hash] functions.
    pub fn set_checkpoints_enabled(&self, enabled: bool) {
        self.checkpoints_enabled.set(enabled)
    }

    /// Creates a new batch containing a single ingress message and sends it for
    /// processing to the replicated state machine.
    fn send_signed_ingress(&self, msg: SignedIngress) {
        self.execute_block_with_ingress_payload(IngressPayload::from(vec![msg]))
    }

    /// Triggers a single round of execution without any new inputs.  The state
    /// machine will invoke hearbeats and make progress on pending async calls.
    pub fn tick(&self) {
        self.execute_block_with_ingress_payload(IngressPayload::default())
    }

    /// Makes the state machine tick until there are no more messages in the system.
    /// This method is useful if you need to wait for asynchronous canister communication to
    /// complete.
    ///
    /// # Panics
    ///
    /// This function panics if the state machine did not process all messages within the
    /// `max_ticks` iterations.
    pub fn run_until_completion(&self, max_ticks: usize) {
        let mut reached_completion = false;
        for _tick in 0..max_ticks {
            let state = self.state_manager.get_latest_state().take();
            reached_completion = !state
                .canisters_iter()
                .any(|canister| canister.has_input() || canister.has_output())
                && !state.subnet_queues().has_input()
                && !state.subnet_queues().has_output();
            if reached_completion {
                break;
            }
            self.tick();
        }
        if !reached_completion {
            panic!(
                "The state machine did not reach completion after {} ticks",
                max_ticks
            );
        }
    }

    fn execute_block_with_ingress_payload(&self, ingress: IngressPayload) {
        let batch_number = self.message_routing.expected_batch_height();

        let mut seed = [0u8; 32];
        // use the batch number to seed randomness
        seed[..8].copy_from_slice(batch_number.get().to_le_bytes().as_slice());

        let batch = Batch {
            batch_number,
            requires_full_state_hash: self.checkpoints_enabled.get(),
            payload: BatchPayload {
                ingress,
                ..BatchPayload::default()
            },
            randomness: Randomness::from(seed),
            ecdsa_subnet_public_keys: BTreeMap::new(),
            registry_version: self.registry_client.get_latest_version(),
            time: self.time.get(),
            consensus_responses: vec![],
        };
        self.message_routing
            .deliver_batch(batch)
            .expect("MR queue overflow");
        self.await_height(batch_number);
    }

    fn await_height(&self, h: Height) {
        const SLEEP_TIME: Duration = Duration::from_millis(100);
        const MAX_WAIT_TIME: Duration = Duration::from_secs(180);

        let started_at = Instant::now();

        while started_at.elapsed() < MAX_WAIT_TIME {
            if self.state_manager.latest_state_height() >= h {
                return;
            }
            std::thread::sleep(SLEEP_TIME);
        }

        panic!(
            "Did not finish executing block {} in {:?}, last executed block: {}",
            h,
            started_at.elapsed(),
            self.state_manager.latest_state_height(),
        )
    }

    /// Returns the total number of Wasm instructions this state machine consumed in replicated
    /// message execution (ingress messages, inter-canister messages, and heartbeats).
    pub fn instructions_consumed(&self) -> f64 {
        fetch_histogram_stats(
            &self.metrics_registry,
            "scheduler_instructions_consumed_per_round",
        )
        .map(|stats| stats.sum)
        .unwrap_or(0.0)
    }

    /// Returns the total number of Wasm instructions executed when executing subnet
    /// messages (IC00 messages addressed to the subnet).
    pub fn subnet_message_instructions(&self) -> f64 {
        fetch_histogram_stats(
            &self.metrics_registry,
            "execution_round_subnet_queue_instructions",
        )
        .map(|stats| stats.sum)
        .unwrap_or(0.0)
    }

    /// Sets the time that the state machine will use for executing next
    /// messages.
    pub fn set_time(&self, time: SystemTime) {
        self.time.set(Time::from_nanos_since_unix_epoch(
            time.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        ));
    }

    /// Returns the current state machine time.
    pub fn time(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(self.time.get().as_nanos_since_unix_epoch())
    }

    /// Advances the state machine time by the given amount.
    pub fn advance_time(&self, amount: Duration) {
        self.set_time(self.time() + amount);
    }

    /// Returns the root key of the state machine.
    pub fn root_key(&self) -> ThresholdSigPublicKey {
        self.public_key
    }

    /// Blocks until the hash of the latest state is computed.
    ///
    /// # Panics
    ///
    /// This function panics if the state hash computation takes more than a few
    /// seconds to complete.
    pub fn await_state_hash(&self) -> CryptoHashOfState {
        let h = self.state_manager.latest_state_height();
        let started_at = Instant::now();
        let mut tries = 0;
        while tries < 100 {
            match self.state_manager.get_state_hash_at(h) {
                Ok(hash) => return hash,
                Err(StateHashError::Transient(_)) => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e @ StateHashError::Permanent(_)) => {
                    panic!("Failed to compute state hash: {}", e)
                }
            }
        }
        panic!(
            "State hash computation took too long ({:?})",
            started_at.elapsed()
        )
    }

    /// Blocks until the result of the ingress message with the specified ID is
    /// available.
    ///
    /// # Panics
    ///
    /// This function panics if the result doesn't become available after the
    /// specified number of state machine ticks.
    pub fn await_ingress(
        &self,
        msg_id: MessageId,
        max_ticks: usize,
    ) -> Result<WasmResult, UserError> {
        let started_at = Instant::now();

        for _tick in 0..max_ticks {
            match self.ingress_status(&msg_id) {
                IngressStatus::Known {
                    state: IngressState::Completed(result),
                    ..
                } => return Ok(result),
                IngressStatus::Known {
                    state: IngressState::Failed(error),
                    ..
                } => return Err(error),
                _ => {
                    self.tick();
                }
            }
        }
        panic!(
            "Did not get answer to ingress {} after {} state machine ticks ({:?} elapsed)",
            msg_id,
            max_ticks,
            started_at.elapsed()
        )
    }

    /// Imports a directory containing a canister snapshot into the state machine.
    ///
    /// After you import the canister, you can execute methods on it and upgrade it.
    /// The original directory is not modified.
    ///
    /// The function is currently not used in code, but it is useful for local
    /// testing and debugging. Do not remove it.
    ///
    /// # Panics
    ///
    /// This function panics if loading the canister snapshot fails.
    pub fn import_canister_state<P: AsRef<Path>>(
        &self,
        canister_directory: P,
        canister_id: CanisterId,
    ) {
        let canister_directory = canister_directory.as_ref();
        assert!(
            canister_directory.is_dir(),
            "canister state at {} must be a directory",
            canister_directory.display()
        );

        let tip = self
            .state_manager
            .state_layout()
            .tip(ic_types::Height::new(0))
            .expect("failed to obtain tip");
        let tip_canister_layout = tip
            .canister(&canister_id)
            .expect("failed to obtain writeable canister layout");

        fn copy_as_writeable(src: &Path, dst: &Path) {
            assert!(
                src.is_file(),
                "Canister layout contains only files, but {} is not a file.",
                src.display()
            );
            std::fs::copy(src, dst).expect("failed to copy file");
            let file = std::fs::File::open(dst).expect("failed to open file");
            let mut permissions = file
                .metadata()
                .expect("failed to get file permission")
                .permissions();
            permissions.set_readonly(false);
            file.set_permissions(permissions)
                .expect("failed to set file persmission");
        }

        for entry in std::fs::read_dir(canister_directory).expect("failed to read_dir") {
            let entry = entry.expect("failed to get directory entry");
            copy_as_writeable(
                &entry.path(),
                &tip_canister_layout.raw_path().join(entry.file_name()),
            );
        }

        let canister_state = ic_state_manager::checkpoint::load_canister_state(
            &tip_canister_layout,
            &canister_id,
            ic_types::Height::new(0),
        )
        .unwrap_or_else(|e| {
            panic!(
                "failed to load canister state from {}: {}",
                canister_directory.display(),
                e
            )
        })
        .0;

        let (h, mut state) = self.state_manager.take_tip();
        state.put_canister_state(canister_state);
        self.state_manager
            .commit_and_certify(state, h.increment(), CertificationScope::Full);
    }

    pub fn install_wasm_in_mode(
        &self,
        canister_id: CanisterId,
        mode: CanisterInstallMode,
        wasm: Vec<u8>,
        payload: Vec<u8>,
    ) -> Result<(), UserError> {
        let state = self.state_manager.get_latest_state().take();
        let sender = state
            .canister_state(&canister_id)
            .and_then(|s| s.controllers().iter().next().cloned())
            .unwrap_or_else(PrincipalId::new_anonymous);
        self.execute_ingress_as(
            sender,
            ic00::IC_00,
            Method::InstallCode,
            InstallCodeArgs::new(mode, canister_id, wasm, payload, None, None, None).encode(),
        )
        .map(|_| ())
    }

    /// Compiles specified WAT to Wasm and installs it for the canister using
    /// the specified ID in the provided install mode.
    fn install_wat_in_mode(
        &self,
        canister_id: CanisterId,
        mode: CanisterInstallMode,
        wat: &str,
        payload: Vec<u8>,
    ) {
        self.install_wasm_in_mode(
            canister_id,
            mode,
            wabt::wat2wasm(wat).expect("invalid WAT"),
            payload,
        )
        .expect("failed to install canister");
    }

    /// Creates a new canister and returns the canister principal.
    pub fn create_canister(&self, settings: Option<CanisterSettingsArgs>) -> CanisterId {
        self.create_canister_with_cycles(Cycles::new(0), settings)
    }

    /// Creates a new canister with a cycles balance and returns the canister principal.
    pub fn create_canister_with_cycles(
        &self,
        cycles: Cycles,
        settings: Option<CanisterSettingsArgs>,
    ) -> CanisterId {
        let wasm_result = self
            .execute_ingress(
                ic00::IC_00,
                ic00::Method::ProvisionalCreateCanisterWithCycles,
                ic00::ProvisionalCreateCanisterWithCyclesArgs {
                    amount: Some(candid::Nat::from(cycles.get())),
                    settings,
                }
                .encode(),
            )
            .expect("failed to create canister");
        match wasm_result {
            WasmResult::Reply(bytes) => CanisterIdRecord::decode(&bytes[..])
                .expect("failed to decode canister ID record")
                .get_canister_id(),
            WasmResult::Reject(reason) => panic!("create_canister call rejected: {}", reason),
        }
    }

    /// Creates a new canister and installs its code.
    /// Returns the ID of the newly created canister.
    ///
    /// This function is synchronous.
    pub fn install_canister(
        &self,
        module: Vec<u8>,
        payload: Vec<u8>,
        settings: Option<CanisterSettingsArgs>,
    ) -> Result<CanisterId, UserError> {
        let canister_id = self.create_canister(settings);
        self.install_wasm_in_mode(canister_id, CanisterInstallMode::Install, module, payload)?;
        Ok(canister_id)
    }

    /// Creates a new canister and installs its code specified by WAT string.
    /// Returns the ID of the newly created canister.
    ///
    /// This function is synchronous.
    ///
    /// # Panics
    ///
    /// Panicks if canister creation or the code install failed.
    pub fn install_canister_wat(
        &self,
        wat: &str,
        payload: Vec<u8>,
        settings: Option<CanisterSettingsArgs>,
    ) -> CanisterId {
        let canister_id = self.create_canister(settings);
        self.install_wat_in_mode(canister_id, CanisterInstallMode::Install, wat, payload);
        canister_id
    }

    /// Erases the previous state and code of the canister with the specified ID
    /// and replaces the code with the compiled form of the provided WAT.
    pub fn reinstall_canister_wat(&self, canister_id: CanisterId, wat: &str, payload: Vec<u8>) {
        self.install_wat_in_mode(canister_id, CanisterInstallMode::Reinstall, wat, payload);
    }

    /// Performs upgrade of the canister with the specified ID to the
    /// code obtained by compiling the provided WAT.
    pub fn upgrade_canister_wat(&self, canister_id: CanisterId, wat: &str, payload: Vec<u8>) {
        self.install_wat_in_mode(canister_id, CanisterInstallMode::Upgrade, wat, payload);
    }

    /// Performs upgrade of the canister with the specified ID to the specified
    /// Wasm code.
    pub fn upgrade_canister(
        &self,
        canister_id: CanisterId,
        wasm: Vec<u8>,
        payload: Vec<u8>,
    ) -> Result<(), UserError> {
        self.install_wasm_in_mode(canister_id, CanisterInstallMode::Upgrade, wasm, payload)
    }

    /// Returns true if the canister with the specified id exists.
    pub fn canister_exists(&self, canister: CanisterId) -> bool {
        self.state_manager
            .get_latest_state()
            .take()
            .canister_states
            .contains_key(&canister)
    }

    /// Queries the canister with the specified ID using the anonymous principal.
    pub fn query(
        &self,
        receiver: CanisterId,
        method: impl ToString,
        method_payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        self.query_as(
            PrincipalId::new_anonymous(),
            receiver,
            method,
            method_payload,
        )
    }

    /// Queries the canister with the specified ID.
    pub fn query_as(
        &self,
        sender: PrincipalId,
        receiver: CanisterId,
        method: impl ToString,
        method_payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        if self.state_manager.latest_state_height() > self.state_manager.latest_certified_height() {
            let state_hashes = self.state_manager.list_state_hashes_to_certify();
            let (height, hash) = state_hashes.last().unwrap();
            self.state_manager
                .deliver_state_certification(self.certify_hash(height, hash));
        }

        let path = SubTree(flatmap! {
            Label::from("canister") => SubTree(
                flatmap! {
                    Label::from(receiver) => SubTree(
                        flatmap!(Label::from("certified_data") => LabeledTree::Leaf(()))
                    )
                }),
            Label::from("time") => LabeledTree::Leaf(())
        });
        let (state, tree, certification) = self.state_manager.read_certified_state(&path).unwrap();
        let data_certificate = into_cbor(&Certificate {
            tree,
            signature: Blob(certification.signed.signature.signature.get().0),
            delegation: None,
        });
        self.query_handler.query(
            UserQuery {
                receiver,
                source: UserId::from(sender),
                method_name: method.to_string(),
                method_payload,
                ingress_expiry: 0,
                nonce: None,
            },
            state,
            data_certificate,
        )
    }

    fn certify_hash(&self, height: &Height, hash: &CryptoHashOfPartialState) -> Certification {
        let signature_bytes = Some(
            sign_message(
                CertificationContent::new(hash.clone())
                    .as_signed_bytes()
                    .as_slice(),
                &self.secret_key,
            )
            .unwrap(),
        );
        let signature = combine_signatures(&[signature_bytes], NumberOfNodes::new(1)).unwrap();
        let combined_sig = CombinedThresholdSigOf::from(CombinedThresholdSig(signature.0.to_vec()));
        Certification {
            height: *height,
            signed: Signed {
                content: CertificationContent { hash: hash.clone() },
                signature: ThresholdSignature {
                    signature: combined_sig,
                    signer: NiDkgId {
                        dealer_subnet: self.subnet_id,
                        target_subnet: NiDkgTargetSubnet::Local,
                        start_block_height: *height,
                        dkg_tag: NiDkgTag::LowThreshold,
                    },
                },
            },
        }
    }

    /// Returns the module hash of the specified canister.
    pub fn module_hash(&self, canister_id: CanisterId) -> Option<[u8; 32]> {
        let state = self.state_manager.get_latest_state().take();
        let canister_state = state.canister_state(&canister_id)?;
        Some(
            canister_state
                .execution_state
                .as_ref()?
                .wasm_binary
                .binary
                .module_hash(),
        )
    }

    /// Executes an ingress message on the canister with the specified ID.
    ///
    /// This function is synchronous, it blocks until the result of the ingress
    /// message is known. The function returns this result.
    ///
    /// # Panics
    ///
    /// This function panics if the status was not ready in a reasonable amount
    /// of time (typically, a few seconds).
    pub fn execute_ingress_as(
        &self,
        sender: PrincipalId,
        canister_id: CanisterId,
        method: impl ToString,
        payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        const MAX_TICKS: usize = 100;
        let msg_id = self.send_ingress(sender, canister_id, method, payload);
        self.await_ingress(msg_id, MAX_TICKS)
    }

    pub fn execute_ingress(
        &self,
        canister_id: CanisterId,
        method: impl ToString,
        payload: Vec<u8>,
    ) -> Result<WasmResult, UserError> {
        self.execute_ingress_as(PrincipalId::new_anonymous(), canister_id, method, payload)
    }

    /// Sends an ingress message to the canister with the specified ID.
    ///
    /// This function is asynchronous. It returns the ID of the ingress message
    /// that can be awaited later with [await_ingress].
    pub fn send_ingress(
        &self,
        sender: PrincipalId,
        canister_id: CanisterId,
        method: impl ToString,
        payload: Vec<u8>,
    ) -> MessageId {
        self.nonce.set(self.nonce.get() + 1);
        let msg = SignedIngress::try_from(HttpRequestEnvelope::<HttpCallContent> {
            content: HttpCallContent::Call {
                update: HttpCanisterUpdate {
                    canister_id: Blob(canister_id.get().into_vec()),
                    method_name: method.to_string(),
                    arg: Blob(payload),
                    sender: Blob(sender.into_vec()),
                    ingress_expiry: current_time_and_expiry_time().1.as_nanos_since_unix_epoch(),
                    nonce: Some(Blob(self.nonce.get().to_be_bytes().to_vec())),
                },
            },
            sender_pubkey: None,
            sender_sig: None,
            sender_delegation: None,
        })
        .unwrap();

        let msg_id = msg.id();
        self.send_signed_ingress(msg);
        msg_id
    }

    /// Returns the status of the ingress message with the specified ID.
    pub fn ingress_status(&self, msg_id: &MessageId) -> IngressStatus {
        (self.ingress_history_reader.get_latest_status())(msg_id)
    }

    /// Stops the canister with the specified ID.
    pub fn stop_canister(&self, canister_id: CanisterId) -> Result<WasmResult, UserError> {
        self.execute_ingress(
            CanisterId::ic_00(),
            "stop_canister",
            (CanisterIdRecord::from(canister_id)).encode(),
        )
    }

    /// Deletes the canister with the specified ID.
    pub fn delete_canister(&self, canister_id: CanisterId) -> Result<WasmResult, UserError> {
        self.execute_ingress(
            CanisterId::ic_00(),
            "delete_canister",
            (CanisterIdRecord::from(canister_id)).encode(),
        )
    }

    /// Updates the routing table so that a range of canisters is assigned to
    /// the specified destination subnet.
    pub fn reroute_canister_range(
        &self,
        canister_range: std::ops::RangeInclusive<CanisterId>,
        destination: SubnetId,
    ) {
        use ic_registry_client_helpers::routing_table::RoutingTableRegistry;

        let last_version = self.registry_client.get_latest_version();
        let next_version = last_version.increment();

        let mut routing_table = self
            .registry_client
            .get_routing_table(last_version)
            .expect("malformed routing table")
            .expect("missing routing table");

        routing_table
            .assign_ranges(
                CanisterIdRanges::try_from(vec![CanisterIdRange {
                    start: *canister_range.start(),
                    end: *canister_range.end(),
                }])
                .unwrap(),
                destination,
            )
            .expect("ranges are not well formed");

        self.registry_data_provider
            .add(
                &make_routing_table_record_key(),
                next_version,
                Some(PbRoutingTable::from(routing_table)),
            )
            .unwrap();
        self.registry_client.update_to_latest_version();

        assert_eq!(next_version, self.registry_client.get_latest_version());
    }

    /// Returns the subnet id of this state machine.
    pub fn get_subnet_id(&self) -> SubnetId {
        self.subnet_id
    }

    /// Marks canisters in the specified range as being migrated to another subnet.
    pub fn prepare_canister_migrations(
        &self,
        canister_range: std::ops::RangeInclusive<CanisterId>,
        source: SubnetId,
        destination: SubnetId,
    ) {
        use ic_registry_client_helpers::routing_table::RoutingTableRegistry;

        let last_version = self.registry_client.get_latest_version();
        let next_version = last_version.increment();

        let mut canister_migrations = self
            .registry_client
            .get_canister_migrations(last_version)
            .expect("malformed canister migrations")
            .unwrap_or_default();

        canister_migrations
            .insert_ranges(
                CanisterIdRanges::try_from(vec![CanisterIdRange {
                    start: *canister_range.start(),
                    end: *canister_range.end(),
                }])
                .unwrap(),
                source,
                destination,
            )
            .expect("ranges are not well formed");

        self.registry_data_provider
            .add(
                &make_canister_migrations_record_key(),
                next_version,
                Some(PbCanisterMigrations::from(canister_migrations)),
            )
            .unwrap();
        self.registry_client.update_to_latest_version();

        assert_eq!(next_version, self.registry_client.get_latest_version());
    }

    /// Marks canisters in the specified range as successfully migrated to another subnet.
    pub fn complete_canister_migrations(
        &self,
        canister_range: std::ops::RangeInclusive<CanisterId>,
        migration_trace: Vec<SubnetId>,
    ) {
        use ic_registry_client_helpers::routing_table::RoutingTableRegistry;

        let last_version = self.registry_client.get_latest_version();
        let next_version = last_version.increment();

        let mut canister_migrations = self
            .registry_client
            .get_canister_migrations(last_version)
            .expect("malformed canister migrations")
            .unwrap_or_default();

        canister_migrations
            .remove_ranges(
                CanisterIdRanges::try_from(vec![CanisterIdRange {
                    start: *canister_range.start(),
                    end: *canister_range.end(),
                }])
                .unwrap(),
                migration_trace,
            )
            .expect("ranges are not well formed");

        self.registry_data_provider
            .add(
                &make_canister_migrations_record_key(),
                next_version,
                Some(PbCanisterMigrations::from(canister_migrations)),
            )
            .unwrap();
        self.registry_client.update_to_latest_version();

        assert_eq!(next_version, self.registry_client.get_latest_version());
    }

    /// Return the subnet_ids from the internal RegistryClient
    pub fn get_subnet_ids(&self) -> Vec<SubnetId> {
        self.registry_client
            .get_subnet_ids(self.registry_client.get_latest_version())
            .unwrap()
            .unwrap()
    }

    /// Returns a stable memory snapshot of the specified canister.
    ///
    /// # Panics
    ///
    /// This function panics if:
    ///   * The specified canister does not exist.
    ///   * The specified canister does not have a module installed.
    pub fn stable_memory(&self, canister_id: CanisterId) -> Vec<u8> {
        let replicated_state = self.state_manager.get_latest_state().take();
        let memory = &replicated_state
            .canister_state(&canister_id)
            .unwrap_or_else(|| panic!("Canister {} does not exist", canister_id))
            .execution_state
            .as_ref()
            .unwrap_or_else(|| panic!("Canister {} has no module", canister_id))
            .stable_memory;

        let mut dst = vec![0u8; memory.size.get() * WASM_PAGE_SIZE_IN_BYTES];
        let buffer = Buffer::new(memory.page_map.clone());
        buffer.read(&mut dst, 0);
        dst
    }

    /// Sets the content of the stable memory for the specified canister.
    ///
    /// If the `data` is not aligned to the Wasm page boundary, this function will extend the stable
    /// memory to have the minimum number of Wasm pages that fit all of the `data`.
    ///
    /// # Notes
    ///
    ///   * Avoid changing the stable memory of arbitrary canisters, they might be not prepared for
    ///     that. Consider upgrading the canister to an empty Wasm module, setting the stable
    ///     memory, and upgrading back to the original module instead.
    ///   * `set_stable_memory(ID, stable_memory(ID))` does not change the canister state.
    ///
    /// # Panics
    ///
    /// This function panics if:
    ///   * The specified canister does not exist.
    ///   * The specified canister does not have a module installed.
    pub fn set_stable_memory(&self, canister_id: CanisterId, data: &[u8]) {
        let (height, mut replicated_state) = self.state_manager.take_tip();
        let canister_state = replicated_state
            .canister_state_mut(&canister_id)
            .unwrap_or_else(|| panic!("Canister {} does not exist", canister_id));
        let size = (data.len() + WASM_PAGE_SIZE_IN_BYTES - 1) / WASM_PAGE_SIZE_IN_BYTES;
        let memory = Memory::new(PageMap::from(data), NumWasmPages::new(size));
        canister_state
            .execution_state
            .as_mut()
            .unwrap_or_else(|| panic!("Canister {} has no module", canister_id))
            .stable_memory = memory;
        self.state_manager.commit_and_certify(
            replicated_state,
            height.increment(),
            CertificationScope::Full,
        );
    }

    /// Returns the cycle balance of the specified canister.
    ///
    /// # Panics
    ///
    /// This function panics if the specified canister does not exist.
    pub fn cycle_balance(&self, canister_id: CanisterId) -> u128 {
        let state = self.state_manager.get_latest_state().take();
        state
            .canister_state(&canister_id)
            .unwrap_or_else(|| panic!("Canister {} not found", canister_id))
            .system_state
            .balance()
            .get()
    }

    /// Tops up the specified canister with cycle amount and returns the resulting cycle balance.
    ///
    /// # Panics
    ///
    /// This function panics if the specified canister does not exist.
    pub fn add_cycles(&self, canister_id: CanisterId, amount: u128) -> u128 {
        let (height, mut state) = self.state_manager.take_tip();
        let canister_state = state
            .canister_state_mut(&canister_id)
            .unwrap_or_else(|| panic!("Canister {} not found", canister_id));
        *canister_state.system_state.balance_mut() += Cycles::from(amount);
        let balance = canister_state.system_state.balance().get();
        self.state_manager
            .commit_and_certify(state, height.increment(), CertificationScope::Full);
        balance
    }
}
