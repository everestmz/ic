use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    convert::{TryFrom, TryInto},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, ThreadId},
};

use ic_base_types::{CanisterId, NumBytes, SubnetId};
use ic_btc_canister::BitcoinCanister;
use ic_config::{
    flag_status::FlagStatus,
    subnet_config::{SchedulerConfig, SubnetConfigs},
};
use ic_cycles_account_manager::CyclesAccountManager;
use ic_embedders::{
    wasm_executor::{
        CanisterStateChanges, PausedWasmExecution, SliceExecutionOutput, WasmExecutionResult,
        WasmExecutor,
    },
    CompilationCache, CompilationResult, WasmExecutionInput,
};
use ic_error_types::UserError;
use ic_ic00_types::{CanisterInstallMode, InstallCodeArgs, Method, Payload};
use ic_interfaces::execution_environment::{
    ExecutionRoundType, HypervisorError, HypervisorResult, IngressHistoryWriter, InstanceStats,
    Scheduler, WasmExecutionOutput,
};
use ic_logger::{replica_logger::no_op_logger, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{
    canister_state::{
        execution_state::{self, WasmMetadata},
        QUEUE_INDEX_NONE,
    },
    testing::{CanisterQueuesTesting, ReplicatedStateTesting},
    CanisterState, ExecutionState, ExportedFunctions, InputQueueType, ReplicatedState,
};
use ic_system_api::{
    sandbox_safe_system_state::{SandboxSafeSystemState, SystemStateChanges},
    ApiType, ExecutionParameters,
};
use ic_test_utilities::{
    execution_environment::test_registry_settings,
    state::CanisterStateBuilder,
    types::{
        ids::{canister_test_id, subnet_test_id, user_test_id},
        messages::{RequestBuilder, SignedIngressBuilder},
    },
};
use ic_types::{
    ingress::{IngressState, IngressStatus},
    messages::{CallContextId, Ingress, MessageId, Request, RequestOrResponse, Response},
    methods::{Callback, FuncRef, SystemMethod, WasmClosure, WasmMethod},
    ComputeAllocation, Cycles, ExecutionRound, MemoryAllocation, NumInstructions, Randomness, Time,
    UserId,
};
use ic_wasm_types::CanisterModule;
use maplit::btreemap;

use crate::{
    as_round_instructions, ExecutionEnvironment, Hypervisor, IngressHistoryWriterImpl, RoundLimits,
};

use super::SchedulerImpl;
use crate::metrics::MeasurementScope;
use ic_crypto::prng::Csprng;
use ic_crypto::prng::RandomnessPurpose::ExecutionThread;
use std::collections::BTreeSet;

/// A helper for the scheduler tests. It comes with its own Wasm executor that
/// fakes execution of Wasm code for performance, so it can process thousands
/// of messages in milliseconds.
///
/// See the comments of `TestMessage` for the description on how to create
/// fake ingress messages and inter-canister call messages.
///
/// Example usages of the test helper:
/// ```
/// let mut test = SchedulerTestBuilder::new().build();
/// let canister_id = test.create_canister();
/// let message = ingress(50);
/// test.send_ingress(canister_id, message);
/// test.execute_round(ExecutionRoundType::OrdinaryRound);
/// ```
pub(crate) struct SchedulerTest {
    // The current replicated state. The option type allows taking the state for
    // execution and then putting it back afterwards.
    state: Option<ReplicatedState>,
    // Monotonically increasing counter used during canister creation.
    next_canister_id: u64,
    // Monotonically increasing counter that specifies the current round.
    round: ExecutionRound,
    // The amount of cycles that new canisters have by default.
    initial_canister_cycles: Cycles,
    // The id of the user that sends ingress messages.
    user_id: UserId,
    // The id of a canister that is guaranteed to be xnet.
    xnet_canister_id: CanisterId,
    // The actual scheduler.
    scheduler: SchedulerImpl,
    // The fake Wasm executor.
    wasm_executor: Arc<TestWasmExecutor>,
}

impl std::fmt::Debug for SchedulerTest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerTest").finish()
    }
}

impl SchedulerTest {
    pub fn state(&self) -> &ReplicatedState {
        self.state.as_ref().unwrap()
    }

    pub fn state_mut(&mut self) -> &mut ReplicatedState {
        self.state.as_mut().unwrap()
    }

    pub fn canister_state(&self, canister_id: CanisterId) -> &CanisterState {
        self.state().canister_state(&canister_id).unwrap()
    }

    pub fn canister_state_mut(&mut self, canister_id: CanisterId) -> &mut CanisterState {
        self.state_mut().canister_state_mut(&canister_id).unwrap()
    }

    pub fn ingress_queue_size(&self, canister_id: CanisterId) -> usize {
        self.canister_state(canister_id)
            .system_state
            .queues()
            .ingress_queue_size()
    }

    pub fn last_round(&self) -> ExecutionRound {
        ExecutionRound::new(self.round.get().max(1) - 1)
    }

    pub fn advance_to_round(&mut self, round: ExecutionRound) {
        self.round = round;
    }

    pub fn scheduler(&self) -> &SchedulerImpl {
        &self.scheduler
    }

    pub fn xnet_canister_id(&self) -> CanisterId {
        self.xnet_canister_id
    }

    /// Returns how many instructions were executed by a canister on a thread
    /// and in an execution round. The order of elements is important and
    /// matches the execution order for a fixed thread.
    pub fn executed_schedule(
        &self,
    ) -> Vec<(ThreadId, ExecutionRound, CanisterId, NumInstructions)> {
        let wasm_executor = self.wasm_executor.core.lock().unwrap();
        wasm_executor.schedule.clone()
    }

    pub fn create_canister(&mut self) -> CanisterId {
        self.create_canister_with(
            self.initial_canister_cycles,
            ComputeAllocation::zero(),
            MemoryAllocation::BestEffort,
            None,
        )
    }

    /// Creates a canister with the given balance and allocations.
    /// The `system_method` parameter can be used to optionally enable the
    /// heartbeat by passing `Some(SystemMethod::CanisterHeartbeat)`.
    /// In that case the heartbeat execution must be specified before each
    /// round using `expect_heartbeat()`.
    pub fn create_canister_with(
        &mut self,
        cycles: Cycles,
        compute_allocation: ComputeAllocation,
        memory_allocation: MemoryAllocation,
        system_method: Option<SystemMethod>,
    ) -> CanisterId {
        let canister_id = self.next_canister_id();
        let wasm_source = system_method
            .map(|x| x.to_string().as_bytes().to_vec())
            .unwrap_or_default();
        let mut canister_state = CanisterStateBuilder::new()
            .with_canister_id(canister_id)
            .with_cycles(cycles)
            .with_controller(self.user_id.get())
            .with_compute_allocation(compute_allocation)
            .with_memory_allocation(memory_allocation.bytes())
            .with_wasm(wasm_source.clone())
            .with_freezing_threshold(100)
            .build();
        let mut wasm_executor = self.wasm_executor.core.lock().unwrap();
        canister_state.execution_state = Some(
            wasm_executor
                .create_execution_state(CanisterModule::new(wasm_source), canister_id)
                .unwrap()
                .0,
        );
        canister_state
            .system_state
            .controllers
            .insert(self.xnet_canister_id.get());
        self.state
            .as_mut()
            .unwrap()
            .put_canister_state(canister_state);
        canister_id
    }

    pub fn send_ingress(&mut self, canister_id: CanisterId, message: TestMessage) -> MessageId {
        let mut wasm_executor = self.wasm_executor.core.lock().unwrap();
        let mut state = self.state.take().unwrap();
        let canister = state.canister_state_mut(&canister_id).unwrap();
        let message_id = wasm_executor.push_ingress(
            canister_id,
            canister,
            message,
            Time::from_nanos_since_unix_epoch(u64::MAX / 2),
        );
        self.state = Some(state);
        message_id
    }

    pub fn ingress_status(&self, message_id: &MessageId) -> IngressStatus {
        self.state.as_ref().unwrap().get_ingress_status(message_id)
    }

    pub fn ingress_error(&self, message_id: &MessageId) -> UserError {
        match self.ingress_status(message_id) {
            IngressStatus::Known { state, .. } => match state {
                IngressState::Failed(error) => error,
                IngressState::Received
                | IngressState::Completed(_)
                | IngressState::Processing
                | IngressState::Done => unreachable!("Unexpected ingress state: {:?}", state),
            },
            IngressStatus::Unknown => unreachable!("Expected message to finish."),
        }
    }

    /// Injects a call to the management canister.
    /// Note that this function doesn't support `InstallCode`
    /// messages, because for such messages we additionally need to know
    /// how many instructions the corresponding Wasm execution needs.
    /// See `inject_install_code_call_to_ic00()`.
    ///
    /// Use `get_responses_to_injected_calls()` to obtain the response
    /// after round execution.
    pub fn inject_call_to_ic00<S: ToString>(
        &mut self,
        method_name: S,
        method_payload: Vec<u8>,
        payment: Cycles,
        caller: CanisterId,
        input_type: InputQueueType,
    ) {
        assert!(
            method_name.to_string() != Method::InstallCode.to_string(),
            "Use `inject_install_code_call_to_ic00()`."
        );

        self.state_mut()
            .subnet_queues_mut()
            .push_input(
                QUEUE_INDEX_NONE,
                RequestBuilder::new()
                    .sender(caller)
                    .receiver(CanisterId::ic_00())
                    .method_name(method_name)
                    .method_payload(method_payload)
                    .payment(payment)
                    .build()
                    .into(),
                input_type,
            )
            .unwrap();
    }

    /// Similar to `inject_call_to_ic00()` but supports `InstallCode` messages.
    /// Example usage:
    /// ```text
    /// let upgrade = TestInstallCode::Upgrade {
    ///     pre_upgrade: instructions(10),
    ///     start: instructions(20),
    ///     post_upgrade: instructions(30),
    /// };
    /// test.inject_install_code_call_to_ic00(canister, upgrade);
    /// ```
    ///
    /// Use `get_responses_to_injected_calls()` to obtain the response
    /// after round execution.
    pub fn inject_install_code_call_to_ic00(
        &mut self,
        target: CanisterId,
        install_code: TestInstallCode,
    ) {
        let wasm_module = wabt::wat2wasm("(module)").unwrap();

        let mode = match &install_code {
            TestInstallCode::Install { .. } => CanisterInstallMode::Install,
            TestInstallCode::Reinstall { .. } => CanisterInstallMode::Reinstall,
            TestInstallCode::Upgrade { .. } => CanisterInstallMode::Upgrade,
        };

        let message_payload = InstallCodeArgs {
            mode,
            canister_id: target.get(),
            wasm_module,
            arg: vec![],
            compute_allocation: None,
            memory_allocation: None,
            query_allocation: None,
        };

        let caller = self.xnet_canister_id();
        self.state_mut()
            .subnet_queues_mut()
            .push_input(
                QUEUE_INDEX_NONE,
                RequestBuilder::new()
                    .sender(caller)
                    .receiver(CanisterId::ic_00())
                    .method_name(Method::InstallCode)
                    .method_payload(message_payload.encode())
                    .build()
                    .into(),
                InputQueueType::RemoteSubnet,
            )
            .unwrap();
        let mut wasm_executor = self.wasm_executor.core.lock().unwrap();
        wasm_executor.push_install_code(target, install_code);
    }

    /// Returns all responses from the management canister to
    /// `self.xnet_canister_id()`.
    pub fn get_responses_to_injected_calls(&mut self) -> Vec<Response> {
        let mut output: Vec<Response> = vec![];
        let xnet_canister_id = self.xnet_canister_id;
        let subnet_queue = self.state_mut().subnet_queues_mut();

        while let Some((_, msg)) = subnet_queue.pop_canister_output(&xnet_canister_id) {
            match msg {
                RequestOrResponse::Request(request) => {
                    panic!(
                        "Expected the xnet message to be a Response, but got a Request: {:?}",
                        request
                    )
                }
                RequestOrResponse::Response(response) => {
                    output.push((*response).clone());
                }
            }
        }
        output
    }

    /// Specifies heartbeat execution for the next round.
    pub fn expect_heartbeat(&mut self, canister_id: CanisterId, heartbeat: TestMessage) {
        assert!(
            self.canister_state(canister_id)
                .execution_state
                .as_ref()
                .unwrap()
                .exports_method(&WasmMethod::System(SystemMethod::CanisterHeartbeat)),
            "The canister should be created with \
             `create_canister_with(.., Some(SystemMethod::Heartbeat))`"
        );
        let mut wasm_executor = self.wasm_executor.core.lock().unwrap();
        wasm_executor.push_heartbeat(canister_id, heartbeat);
    }

    pub fn execute_round(&mut self, round_type: ExecutionRoundType) {
        let state = self.state.take().unwrap();
        let state = self.scheduler.execute_round(
            state,
            Randomness::from([0; 32]),
            BTreeMap::new(),
            self.round,
            round_type,
            &test_registry_settings(),
        );
        self.state = Some(state);
        self.increment_round();
    }

    pub fn drain_subnet_messages(
        &mut self,
        long_running_canister_ids: &BTreeSet<CanisterId>,
    ) -> ReplicatedState {
        let state = self.state.take().unwrap();
        let mut csprng = Csprng::from_seed_and_purpose(
            &Randomness::from([0; 32]),
            &ExecutionThread(self.scheduler.config.scheduler_cores as u32),
        );
        let mut round_limits = RoundLimits {
            instructions: as_round_instructions(
                self.scheduler.config.max_instructions_per_round / 16,
            ),
            subnet_available_memory: self
                .scheduler
                .exec_env
                .subnet_available_memory(&state)
                .into(),
        };
        let measurements = MeasurementScope::root(&self.scheduler.metrics.round_subnet_queue);
        self.scheduler.drain_subnet_queues(
            state,
            &mut csprng,
            &mut round_limits,
            &measurements,
            long_running_canister_ids,
            &test_registry_settings(),
            &BTreeMap::new(),
        )
    }

    pub fn induct_messages_on_same_subnet(&mut self) {
        self.scheduler
            .induct_messages_on_same_subnet(self.state.as_mut().unwrap());
    }

    fn increment_round(&mut self) {
        let mut wasm_executor = self.wasm_executor.core.lock().unwrap();
        self.round = ExecutionRound::new(self.round.get() + 1);
        wasm_executor.round = self.round;
    }

    fn next_canister_id(&mut self) -> CanisterId {
        let canister_id = canister_test_id(self.next_canister_id);
        self.next_canister_id += 1;
        canister_id
    }
}

/// A builder for `SchedulerTest`.
pub(crate) struct SchedulerTestBuilder {
    own_subnet_id: SubnetId,
    nns_subnet_id: SubnetId,
    subnet_type: SubnetType,
    scheduler_config: SchedulerConfig,
    initial_canister_cycles: Cycles,
    subnet_total_memory: u64,
    subnet_message_memory: u64,
    max_canister_memory_size: u64,
    allocatable_compute_capacity_in_percent: usize,
    rate_limiting_of_instructions: bool,
    rate_limiting_of_heap_delta: bool,
    deterministic_time_slicing: bool,
    log: ReplicaLogger,
}

impl Default for SchedulerTestBuilder {
    fn default() -> Self {
        let subnet_type = SubnetType::Application;
        let scheduler_config = SubnetConfigs::default()
            .own_subnet_config(subnet_type)
            .scheduler_config;
        let config = ic_config::execution_environment::Config::default();
        let subnet_total_memory = config.subnet_memory_capacity.get();
        let max_canister_memory_size = config.max_canister_memory_size.get();
        Self {
            own_subnet_id: subnet_test_id(1),
            nns_subnet_id: subnet_test_id(2),
            subnet_type,
            scheduler_config,
            initial_canister_cycles: Cycles::new(1_000_000_000_000_000_000),
            subnet_total_memory,
            subnet_message_memory: subnet_total_memory,
            max_canister_memory_size,
            allocatable_compute_capacity_in_percent: 100,
            rate_limiting_of_instructions: false,
            rate_limiting_of_heap_delta: false,
            deterministic_time_slicing: false,
            log: no_op_logger(),
        }
    }
}

impl SchedulerTestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_subnet_type(self, subnet_type: SubnetType) -> Self {
        let scheduler_config = SubnetConfigs::default()
            .own_subnet_config(subnet_type)
            .scheduler_config;
        Self {
            subnet_type,
            scheduler_config,
            ..self
        }
    }

    pub fn with_subnet_total_memory(self, subnet_total_memory: u64) -> Self {
        Self {
            subnet_total_memory,
            ..self
        }
    }

    pub fn with_subnet_message_memory(self, subnet_message_memory: u64) -> Self {
        Self {
            subnet_message_memory,
            ..self
        }
    }

    pub fn with_max_canister_memory_size(self, max_canister_memory_size: u64) -> Self {
        Self {
            max_canister_memory_size,
            ..self
        }
    }

    pub fn with_scheduler_config(self, scheduler_config: SchedulerConfig) -> Self {
        Self {
            scheduler_config,
            ..self
        }
    }

    pub fn with_rate_limiting_of_instructions(self) -> Self {
        Self {
            rate_limiting_of_instructions: true,
            ..self
        }
    }

    pub fn with_rate_limiting_of_heap_delta(self) -> Self {
        Self {
            rate_limiting_of_heap_delta: true,
            ..self
        }
    }

    pub fn with_deterministic_time_slicing(self) -> Self {
        Self {
            deterministic_time_slicing: true,
            ..self
        }
    }

    pub fn build(self) -> SchedulerTest {
        let tmpdir = tempfile::Builder::new().prefix("test").tempdir().unwrap();
        let first_xnet_canister = u64::MAX / 2;
        let routing_table = Arc::new(
            RoutingTable::try_from(btreemap! {
                CanisterIdRange { start: CanisterId::from(0x0), end: CanisterId::from(first_xnet_canister) } => self.own_subnet_id,
            }).unwrap()
        );

        let mut state = ReplicatedState::new_rooted_at(
            self.own_subnet_id,
            self.subnet_type,
            tmpdir.path().to_path_buf(),
        );
        state.metadata.network_topology.routing_table = routing_table;
        state.metadata.network_topology.nns_subnet_id = self.nns_subnet_id;

        let metrics_registry = MetricsRegistry::new();

        let config = SubnetConfigs::default()
            .own_subnet_config(self.subnet_type)
            .cycles_account_manager_config;
        let cycles_account_manager = Arc::new(CyclesAccountManager::new(
            self.scheduler_config.max_instructions_per_message,
            self.subnet_type,
            self.own_subnet_id,
            config,
        ));
        let rate_limiting_of_instructions = if self.rate_limiting_of_instructions {
            FlagStatus::Enabled
        } else {
            FlagStatus::Disabled
        };
        let rate_limiting_of_heap_delta = if self.rate_limiting_of_heap_delta {
            FlagStatus::Enabled
        } else {
            FlagStatus::Disabled
        };
        let deterministic_time_slicing = if self.deterministic_time_slicing {
            FlagStatus::Enabled
        } else {
            FlagStatus::Disabled
        };
        let config = ic_config::execution_environment::Config {
            allocatable_compute_capacity_in_percent: self.allocatable_compute_capacity_in_percent,
            subnet_memory_capacity: NumBytes::from(self.subnet_total_memory as u64),
            subnet_message_memory_capacity: NumBytes::from(self.subnet_message_memory as u64),
            max_canister_memory_size: NumBytes::from(self.max_canister_memory_size),
            rate_limiting_of_instructions,
            rate_limiting_of_heap_delta,
            deterministic_time_slicing,
            ..ic_config::execution_environment::Config::default()
        };
        let wasm_executor = Arc::new(TestWasmExecutor::new());
        let hypervisor = Hypervisor::new_for_testing(
            &metrics_registry,
            self.own_subnet_id,
            self.subnet_type,
            self.log.clone(),
            Arc::clone(&cycles_account_manager),
            Arc::<TestWasmExecutor>::clone(&wasm_executor),
            deterministic_time_slicing,
            config.cost_to_compile_wasm_instruction,
        );
        let hypervisor = Arc::new(hypervisor);
        let ingress_history_writer =
            IngressHistoryWriterImpl::new(config.clone(), self.log.clone(), &metrics_registry);
        let ingress_history_writer: Arc<dyn IngressHistoryWriter<State = ReplicatedState>> =
            Arc::new(ingress_history_writer);
        let exec_env = ExecutionEnvironment::new(
            self.log.clone(),
            hypervisor,
            Arc::clone(&ingress_history_writer),
            &metrics_registry,
            self.own_subnet_id,
            self.subnet_type,
            1,
            config,
            Arc::clone(&cycles_account_manager),
        );
        let bitcoin_canister = Arc::new(BitcoinCanister::new(&metrics_registry, self.log.clone()));
        let scheduler = SchedulerImpl::new(
            self.scheduler_config,
            self.own_subnet_id,
            ingress_history_writer,
            Arc::new(exec_env),
            cycles_account_manager,
            bitcoin_canister,
            &metrics_registry,
            self.log,
            rate_limiting_of_heap_delta,
            rate_limiting_of_instructions,
            deterministic_time_slicing,
        );
        SchedulerTest {
            state: Some(state),
            next_canister_id: 0,
            round: ExecutionRound::new(0),
            initial_canister_cycles: self.initial_canister_cycles,
            user_id: user_test_id(1),
            xnet_canister_id: canister_test_id(first_xnet_canister),
            scheduler,
            wasm_executor,
        }
    }
}

/// A test message specifies the results returned when the message is executed
/// by the fake Wasm executor:
/// - the number of instructions consumed by execution.
/// - the number of dirty pages produced by execution.
/// - outgoing calls to other canisters produced by execution.
///
/// A test message can be constructed using the helper functions defined below:
/// - `ingress(5)`: a message that uses 5 instructions.
/// - `ingress(5).dirty_pages(1): a message that uses 5 instructions and
///    modifies one page.
/// - `ingress(5).call(other_side(callee, 3), on_response(8))`: a message
///    that uses 5 instructions and calls a canister with id `callee`.
///    The called message uses 3 instructions. The response handler  uses
///    8 instructions.
#[derive(Clone, Debug)]
pub(crate) struct TestMessage {
    // The canister id is optional and is inferred from the context if not
    // provided.
    canister: Option<CanisterId>,
    // The number of instructions that execution of this message will use.
    instructions: NumInstructions,
    // The number of 4KiB pages that execution of this message will writes to.
    dirty_pages: usize,
    // The outgoing calls that will be produced by execution of this message.
    calls: Vec<TestCall>,
}

impl TestMessage {
    pub fn dirty_pages(self, dirty_pages: usize) -> TestMessage {
        Self {
            dirty_pages,
            ..self
        }
    }
    pub fn call(mut self, other_side: TestMessage, on_response: TestMessage) -> TestMessage {
        self.calls.push(TestCall {
            other_side,
            on_response,
        });
        self
    }
}

// An internal helper struct to store the description of an inter-canister call.
#[derive(Clone, Debug)]
struct TestCall {
    // The message to execute on the callee side.
    other_side: TestMessage,
    // The response handler to execute on the caller side.
    on_response: TestMessage,
}

/// Description of an `install_code` message.
#[derive(Clone, Debug)]
pub(crate) enum TestInstallCode {
    Install {
        start: TestMessage,
        init: TestMessage,
    },
    Reinstall {
        start: TestMessage,
        init: TestMessage,
    },
    Upgrade {
        pre_upgrade: TestMessage,
        start: TestMessage,
        post_upgrade: TestMessage,
    },
}

/// A helper to create an ingress test message. Note that the canister id is not
/// needed and will be specified by the function that enqueues the ingress.
pub(crate) fn ingress(instructions: u64) -> TestMessage {
    TestMessage {
        canister: None,
        instructions: NumInstructions::from(instructions),
        dirty_pages: 0,
        calls: vec![],
    }
}

/// A helper to create the test message of the callee.
pub(crate) fn other_side(callee: CanisterId, instructions: u64) -> TestMessage {
    TestMessage {
        canister: Some(callee),
        instructions: NumInstructions::from(instructions),
        dirty_pages: 0,
        calls: vec![],
    }
}

/// A helper to create the test message for handling the response of a call.
/// Note that the canister id is not needed and is inferred from the context.
pub(crate) fn on_response(instructions: u64) -> TestMessage {
    TestMessage {
        canister: None,
        instructions: NumInstructions::from(instructions),
        dirty_pages: 0,
        calls: vec![],
    }
}

/// A generic helper to describe a phase like `start`, `init`, `pre_upgrade`,
/// `post_upgrade` of an install code message.
pub(crate) fn instructions(instructions: u64) -> TestMessage {
    TestMessage {
        canister: None,
        instructions: NumInstructions::from(instructions),
        dirty_pages: 0,
        calls: vec![],
    }
}

// A wrapper around the fake Wasm executor.
// This wrapper is needs to guaranteed thread-safety.
struct TestWasmExecutor {
    core: Mutex<TestWasmExecutorCore>,
}

impl TestWasmExecutor {
    fn new() -> Self {
        Self {
            core: Mutex::new(TestWasmExecutorCore::new()),
        }
    }
}

impl WasmExecutor for TestWasmExecutor {
    // The entry point of the Wasm executor.
    //
    // It finds the test message corresponding to the given input and "executes"
    // it by interpreting its description.
    fn execute(
        self: Arc<Self>,
        input: WasmExecutionInput,
        execution_state: &ExecutionState,
    ) -> (Option<CompilationResult>, WasmExecutionResult) {
        let (_message_id, message, call_context_id) = {
            let mut guard = self.core.lock().unwrap();
            guard.take_message(&input)
        };
        let execution = TestPausedWasmExecution {
            message,
            sandbox_safe_system_state: input.sandbox_safe_system_state,
            execution_parameters: input.execution_parameters,
            canister_current_memory_usage: input.canister_current_memory_usage,
            call_context_id,
            instructions_executed: NumInstructions::from(0),
            executor: Arc::clone(&self),
        };
        let result = Box::new(execution).resume(execution_state);
        (None, result)
    }

    fn create_execution_state(
        &self,
        canister_module: CanisterModule,
        _canister_root: PathBuf,
        canister_id: CanisterId,
        _compilation_cache: Arc<CompilationCache>,
    ) -> HypervisorResult<(ExecutionState, NumInstructions, Option<CompilationResult>)> {
        let mut guard = self.core.lock().unwrap();
        guard.create_execution_state(canister_module, canister_id)
    }
}

// A fake Wasm executor that works as follows:
// - The test helper registers incoming test messages with this executor.
// - Each registered test message has a unique `u32` id.
// - For each registered test message, the corresponding real message
//   is created such that the test message id is encoded in the real message:
//   either in the payload (for calls) or in the environment of the callback
//   (for reply/reject).
// - In the `execute` function, the executor looks up the corresponding
//   test message and interprets its description.
struct TestWasmExecutorCore {
    messages: HashMap<u32, TestMessage>,
    install_code: HashMap<CanisterId, VecDeque<TestInstallCode>>,
    current_install_code: Option<TestInstallCode>,
    heartbeat: HashMap<CanisterId, VecDeque<TestMessage>>,
    schedule: Vec<(ThreadId, ExecutionRound, CanisterId, NumInstructions)>,
    next_message_id: u32,
    round: ExecutionRound,
}

impl TestWasmExecutorCore {
    fn new() -> Self {
        Self {
            messages: HashMap::new(),
            install_code: HashMap::new(),
            current_install_code: None,
            heartbeat: HashMap::new(),
            schedule: vec![],
            next_message_id: 0,
            round: ExecutionRound::new(0),
        }
    }

    // Advances progress of the given paused execution by executing one slice.
    fn execute_slice(
        &mut self,
        mut paused: Box<TestPausedWasmExecution>,
        execution_state: &ExecutionState,
    ) -> WasmExecutionResult {
        let thread_id = thread::current().id();
        let canister_id = paused.sandbox_safe_system_state.canister_id();

        let message_limit = paused.execution_parameters.instruction_limits.message();
        let slice_limit = paused.execution_parameters.instruction_limits.slice();
        let instructions_to_execute =
            paused.message.instructions.min(message_limit) - paused.instructions_executed;

        let is_last_slice = instructions_to_execute <= slice_limit;
        if !is_last_slice {
            paused.instructions_executed += slice_limit;
            let slice = SliceExecutionOutput {
                executed_instructions: slice_limit,
            };
            self.schedule
                .push((thread_id, self.round, canister_id, slice_limit));
            return WasmExecutionResult::Paused(slice, paused);
        }

        paused.instructions_executed += instructions_to_execute;

        if paused.message.instructions > message_limit {
            let slice = SliceExecutionOutput {
                executed_instructions: instructions_to_execute,
            };
            let output = WasmExecutionOutput {
                wasm_result: Err(HypervisorError::InstructionLimitExceeded),
                num_instructions_left: NumInstructions::from(0),
                allocated_bytes: NumBytes::from(0),
                allocated_message_bytes: NumBytes::from(0),
                instance_stats: InstanceStats {
                    accessed_pages: 0,
                    dirty_pages: 0,
                },
            };
            self.schedule
                .push((thread_id, self.round, canister_id, instructions_to_execute));
            return WasmExecutionResult::Finished(slice, output, None);
        }

        let message = paused.message;
        let instructions_left = message_limit - paused.instructions_executed;

        // Generate all the outgoing calls.
        let system_state_changes = self.perform_calls(
            paused.sandbox_safe_system_state,
            message.calls,
            paused.call_context_id,
            paused.canister_current_memory_usage,
            paused.execution_parameters.compute_allocation,
        );

        let canister_state_changes = CanisterStateChanges {
            globals: execution_state.exported_globals.clone(),
            wasm_memory: execution_state.wasm_memory.clone(),
            stable_memory: execution_state.stable_memory.clone(),
            system_state_changes,
        };

        let instance_stats = InstanceStats {
            accessed_pages: message.dirty_pages,
            dirty_pages: message.dirty_pages,
        };
        let slice = SliceExecutionOutput {
            executed_instructions: instructions_to_execute,
        };
        let output = WasmExecutionOutput {
            wasm_result: Ok(None),
            allocated_bytes: NumBytes::from(0),
            allocated_message_bytes: NumBytes::from(0),
            num_instructions_left: instructions_left,
            instance_stats,
        };
        self.schedule
            .push((thread_id, self.round, canister_id, instructions_to_execute));
        WasmExecutionResult::Finished(slice, output, Some(canister_state_changes))
    }

    fn create_execution_state(
        &mut self,
        canister_module: CanisterModule,
        _canister_id: CanisterId,
    ) -> HypervisorResult<(ExecutionState, NumInstructions, Option<CompilationResult>)> {
        let mut exported_functions = vec![
            WasmMethod::Update("update".into()),
            WasmMethod::System(SystemMethod::CanisterPreUpgrade),
            WasmMethod::System(SystemMethod::CanisterStart),
            WasmMethod::System(SystemMethod::CanisterPostUpgrade),
            WasmMethod::System(SystemMethod::CanisterInit),
        ];
        if !canister_module.as_slice().is_empty() {
            if let Ok(text) = std::str::from_utf8(canister_module.as_slice()) {
                if let Ok(system_method) = SystemMethod::try_from(text) {
                    exported_functions.push(WasmMethod::System(system_method));
                }
            }
        }
        let execution_state = ExecutionState::new(
            Default::default(),
            execution_state::WasmBinary::new(canister_module),
            ExportedFunctions::new(exported_functions.into_iter().collect()),
            Default::default(),
            Default::default(),
            vec![],
            WasmMetadata::default(),
        );
        let compilation_result = CompilationResult::empty_for_testing();
        Ok((
            execution_state,
            NumInstructions::from(0),
            Some(compilation_result),
        ))
    }

    fn perform_calls(
        &mut self,
        mut system_state: SandboxSafeSystemState,
        calls: Vec<TestCall>,
        call_context_id: Option<CallContextId>,
        canister_current_memory_usage: NumBytes,
        compute_allocation: ComputeAllocation,
    ) -> SystemStateChanges {
        for call in calls.into_iter() {
            if let Err(error) = self.perform_call(
                &mut system_state,
                call,
                call_context_id.unwrap(),
                canister_current_memory_usage,
                compute_allocation,
            ) {
                eprintln!("Skipping a call due to an error: {}", error);
            }
        }
        system_state.take_changes()
    }

    // Create the request and callback corresponding to the given test call.
    fn perform_call(
        &mut self,
        system_state: &mut SandboxSafeSystemState,
        call: TestCall,
        call_context_id: CallContextId,
        canister_current_memory_usage: NumBytes,
        compute_allocation: ComputeAllocation,
    ) -> Result<(), String> {
        let sender = system_state.canister_id();
        let receiver = call.other_side.canister.unwrap();
        let call_message_id = self.next_message_id();
        let response_message_id = self.next_message_id();
        let closure = WasmClosure {
            func_idx: 0,
            env: response_message_id,
        };
        let callback = system_state
            .register_callback(Callback {
                call_context_id,
                originator: Some(sender),
                respondent: Some(receiver),
                cycles_sent: Cycles::zero(),
                on_reply: closure.clone(),
                on_reject: closure,
                on_cleanup: None,
            })
            .map_err(|err| err.to_string())?;
        let request = Request {
            receiver,
            sender,
            sender_reply_callback: callback,
            payment: Cycles::zero(),
            method_name: "update".into(),
            method_payload: encode_message_id_as_payload(call_message_id),
        };
        system_state
            .push_output_request(canister_current_memory_usage, compute_allocation, request)
            .map_err(|req| format!("Failed pushing request {:?} to output queue.", req))?;
        self.messages.insert(call_message_id, call.other_side);
        self.messages.insert(response_message_id, call.on_response);
        Ok(())
    }

    // Returns the test message corresponding to the given input.
    fn take_message(
        &mut self,
        input: &WasmExecutionInput,
    ) -> (u32, TestMessage, Option<CallContextId>) {
        let canister_id = input.sandbox_safe_system_state.canister_id();
        match &input.api_type {
            ApiType::Update {
                incoming_payload,
                call_context_id,
                ..
            } => {
                let message_id = decode_message_id_from_payload(incoming_payload.clone());
                let message = self.messages.remove(&message_id).unwrap();
                (message_id, message, Some(*call_context_id))
            }
            ApiType::ReplyCallback {
                call_context_id, ..
            }
            | ApiType::RejectCallback {
                call_context_id, ..
            } => {
                let message_id = match &input.func_ref {
                    FuncRef::Method(_) => unreachable!("A callback requires a closure"),
                    FuncRef::UpdateClosure(closure) | FuncRef::QueryClosure(closure) => closure.env,
                };
                let message = self.messages.remove(&message_id).unwrap();
                (message_id, message, Some(*call_context_id))
            }
            ApiType::Heartbeat {
                call_context_id, ..
            } => {
                let message_id = self.next_message_id();
                let message = self
                    .heartbeat
                    .get_mut(&canister_id)
                    .unwrap()
                    .pop_front()
                    .unwrap();
                (message_id, message, Some(*call_context_id))
            }
            ApiType::Start => {
                let message_id = self.next_message_id();
                let message = match self.current_install_code.clone() {
                    Some(TestInstallCode::Upgrade { start, .. }) => start,
                    _ => {
                        // Starting a new `install_code`, get it from the deque.
                        let install_code = self
                            .install_code
                            .get_mut(&canister_id)
                            .unwrap()
                            .pop_front()
                            .unwrap();
                        self.current_install_code = Some(install_code.clone());
                        match install_code {
                            TestInstallCode::Install { start, .. }
                            | TestInstallCode::Reinstall { start, .. } => start,
                            TestInstallCode::Upgrade { .. } => {
                                unreachable!("Executing `start` before `pre_upgrade`")
                            }
                        }
                    }
                };
                (message_id, message, None)
            }
            ApiType::Init { .. } => {
                let message_id = self.next_message_id();
                let install_code = self.current_install_code.take().unwrap();
                let message = match install_code {
                    TestInstallCode::Install { init, .. }
                    | TestInstallCode::Reinstall { init, .. } => init,
                    TestInstallCode::Upgrade { post_upgrade, .. } => {
                        // `ApiType::Init` is reused for `post_upgrade`.
                        post_upgrade
                    }
                };
                (message_id, message, None)
            }
            ApiType::PreUpgrade { .. } => {
                let message_id = self.next_message_id();
                // Starting a new `install_code`, get it from the deque.
                let install_code = self
                    .install_code
                    .get_mut(&canister_id)
                    .unwrap()
                    .pop_front()
                    .unwrap();
                self.current_install_code = Some(install_code.clone());
                let message = match install_code {
                    TestInstallCode::Install { .. } | TestInstallCode::Reinstall { .. } => {
                        unreachable!("Requested pre_upgrade for (re-)install")
                    }
                    TestInstallCode::Upgrade { pre_upgrade, .. } => pre_upgrade,
                };
                (message_id, message, None)
            }
            ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Cleanup { .. } => {
                unreachable!("The test Wasm executor does not support {}", input.api_type)
            }
        }
    }

    fn push_ingress(
        &mut self,
        canister_id: CanisterId,
        canister: &mut CanisterState,
        message: TestMessage,
        expiry_time: Time,
    ) -> MessageId {
        let ingress_id = self.next_message_id();
        self.messages.insert(ingress_id, message);
        let ingress: Ingress = (
            SignedIngressBuilder::new()
                .canister_id(canister_id)
                .method_name("update")
                .method_payload(encode_message_id_as_payload(ingress_id))
                .expiry_time(expiry_time)
                .build(),
            None,
        )
            .into();
        let message_id = ingress.message_id.clone();
        canister.push_ingress(ingress);
        message_id
    }

    fn push_install_code(&mut self, canister_id: CanisterId, install_code: TestInstallCode) {
        self.install_code
            .entry(canister_id)
            .or_default()
            .push_back(install_code);
    }

    fn push_heartbeat(&mut self, canister_id: CanisterId, heartbeat: TestMessage) {
        self.heartbeat
            .entry(canister_id)
            .or_default()
            .push_back(heartbeat);
    }

    fn next_message_id(&mut self) -> u32 {
        let result = self.next_message_id;
        self.next_message_id += 1;
        result
    }
}

/// Represent fake Wasm execution that can be paused and resumed.
struct TestPausedWasmExecution {
    message: TestMessage,
    sandbox_safe_system_state: SandboxSafeSystemState,
    execution_parameters: ExecutionParameters,
    canister_current_memory_usage: NumBytes,
    call_context_id: Option<CallContextId>,
    instructions_executed: NumInstructions,
    executor: Arc<TestWasmExecutor>,
}

impl std::fmt::Debug for TestPausedWasmExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestPausedWasmExecution")
            .field("message", &self.message)
            .field("instructions_executed", &self.instructions_executed)
            .finish()
    }
}

impl PausedWasmExecution for TestPausedWasmExecution {
    fn resume(self: Box<Self>, execution_state: &ExecutionState) -> WasmExecutionResult {
        let executor = Arc::clone(&self.executor);
        let mut guard = executor.core.lock().unwrap();
        guard.execute_slice(self, execution_state)
    }

    fn abort(self: Box<Self>) {
        // Nothing to do.
    }
}

fn decode_message_id_from_payload(payload: Vec<u8>) -> u32 {
    u32::from_le_bytes(payload.try_into().unwrap())
}

fn encode_message_id_as_payload(message_id: u32) -> Vec<u8> {
    message_id.to_le_bytes().into()
}
