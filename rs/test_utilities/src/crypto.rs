pub mod basic_utilities;
pub mod fake_tls_handshake;

pub use ic_crypto_test_utils::files as temp_dir;

use crate::types::ids::node_test_id;
use ic_crypto::utils::TempCryptoComponent;
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::CspNiDkgDealing;
use ic_crypto_test_utils_canister_threshold_sigs::{
    create_params_for_dealers, mock_transcript, mock_unmasked_transcript_type, set_of_nodes,
};
use ic_interfaces::crypto::{
    BasicSigVerifier, BasicSigVerifierByPublicKey, BasicSigner, CanisterSigVerifier, IDkgProtocol,
    KeyManager, LoadTranscriptResult, NiDkgAlgorithm, PublicKeyRegistrationStatus,
    ThresholdEcdsaSigVerifier, ThresholdEcdsaSigner, ThresholdSigVerifier,
    ThresholdSigVerifierByPublicKey, ThresholdSigner,
};
use ic_interfaces::crypto::{MultiSigVerifier, MultiSigner, Signable};
use ic_interfaces::registry::RegistryClient;
use ic_protobuf::crypto::v1::NodePublicKeys;
use ic_registry_client_fake::FakeRegistryClient;
use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
use ic_types::crypto::canister_threshold_sig::error::*;
use ic_types::crypto::canister_threshold_sig::idkg::*;
use ic_types::crypto::canister_threshold_sig::*;
use ic_types::crypto::threshold_sig::ni_dkg::errors::create_dealing_error::DkgCreateDealingError;
use ic_types::crypto::threshold_sig::ni_dkg::errors::create_transcript_error::DkgCreateTranscriptError;
use ic_types::crypto::threshold_sig::ni_dkg::errors::key_removal_error::DkgKeyRemovalError;
use ic_types::crypto::threshold_sig::ni_dkg::errors::load_transcript_error::DkgLoadTranscriptError;
use ic_types::crypto::threshold_sig::ni_dkg::errors::verify_dealing_error::DkgVerifyDealingError;
use ic_types::crypto::threshold_sig::ni_dkg::{
    config::NiDkgConfig, DkgId, NiDkgDealing, NiDkgId, NiDkgTranscript,
};
use ic_types::crypto::{
    AlgorithmId, BasicSig, BasicSigOf, CanisterSigOf, CombinedMultiSig, CombinedMultiSigOf,
    CombinedThresholdSig, CombinedThresholdSigOf, CryptoResult, IndividualMultiSig,
    IndividualMultiSigOf, ThresholdSigShare, ThresholdSigShareOf, UserPublicKey,
};
use ic_types::signature::{BasicSignature, BasicSignatureBatch};
use ic_types::*;
use ic_types::{NodeId, RegistryVersion};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

pub fn empty_fake_registry() -> Arc<dyn RegistryClient> {
    Arc::new(FakeRegistryClient::new(Arc::new(
        ProtoRegistryDataProvider::new(),
    )))
}

pub fn temp_crypto_component_with_fake_registry(node_id: NodeId) -> TempCryptoComponent {
    TempCryptoComponent::new(empty_fake_registry(), node_id)
}

fn empty_ni_dkg_csp_dealing() -> CspNiDkgDealing {
    ic_crypto_test_utils::dkg::ni_dkg_csp_dealing(0)
}

fn empty_ni_dkg_dealing() -> NiDkgDealing {
    NiDkgDealing {
        internal_dealing: empty_ni_dkg_csp_dealing(),
    }
}

pub use ic_crypto_test_utils::dkg::empty_ni_dkg_transcripts_with_committee;

pub fn dummy_idkg_transcript_id_for_tests(id: u64) -> IDkgTranscriptId {
    let subnet = SubnetId::from(PrincipalId::new_subnet_test_id(314159));
    let height = Height::new(42);
    IDkgTranscriptId::new(subnet, id, height)
}

pub fn dummy_idkg_dealing_for_tests() -> IDkgDealing {
    IDkgDealing {
        transcript_id: IDkgTranscriptId::new(
            SubnetId::from(PrincipalId::new_subnet_test_id(1)),
            1,
            Height::new(1),
        ),
        internal_dealing_raw: vec![],
    }
}

pub fn dummy_initial_idkg_dealing_for_tests() -> InitialIDkgDealings {
    let previous_receivers = set_of_nodes(&[35, 36, 37, 38]);
    let previous_transcript =
        mock_transcript(Some(previous_receivers), mock_unmasked_transcript_type());
    let dealers = set_of_nodes(&[35, 36, 38]);

    // For a Resharing Unmasked transcript, the dealer set should be a subset of the previous receiver set.
    assert!(dealers.is_subset(previous_transcript.receivers.get()));

    let params = create_params_for_dealers(
        &dealers,
        IDkgTranscriptOperation::ReshareOfUnmasked(previous_transcript),
    );
    let dealings = mock_dealings(params.transcript_id(), &dealers);

    InitialIDkgDealings::new(params, dealings)
        .expect("Failed creating IDkgInitialDealings for testing")
}

pub fn dummy_idkg_complaint_for_tests() -> IDkgComplaint {
    IDkgComplaint {
        transcript_id: IDkgTranscriptId::new(
            SubnetId::from(PrincipalId::new_subnet_test_id(1)),
            1,
            Height::new(1),
        ),
        dealer_id: NodeId::from(PrincipalId::new_node_test_id(0)),
        internal_complaint_raw: vec![],
    }
}

pub fn dummy_idkg_opening_for_tests(complaint: &IDkgComplaint) -> IDkgOpening {
    IDkgOpening {
        transcript_id: complaint.transcript_id,
        dealer_id: complaint.dealer_id,
        internal_opening_raw: vec![],
    }
}

pub fn dummy_sig_inputs_for_tests(caller: PrincipalId) -> ThresholdEcdsaSigInputs {
    let (fake_key, fake_presig_quadruple) = {
        let mut nodes = BTreeSet::new();
        nodes.insert(node_test_id(1));

        let original_kappa_id = dummy_idkg_transcript_id_for_tests(1);
        let kappa_id = dummy_idkg_transcript_id_for_tests(2);
        let lambda_id = dummy_idkg_transcript_id_for_tests(3);
        let key_id = dummy_idkg_transcript_id_for_tests(4);

        let fake_kappa = IDkgTranscript {
            transcript_id: kappa_id,
            receivers: IDkgReceivers::new(nodes.clone()).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: BTreeMap::new(),
            transcript_type: IDkgTranscriptType::Unmasked(
                IDkgUnmaskedTranscriptOrigin::ReshareMasked(original_kappa_id),
            ),
            algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
            internal_transcript_raw: vec![],
        };

        let fake_lambda = IDkgTranscript {
            transcript_id: lambda_id,
            receivers: IDkgReceivers::new(nodes.clone()).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: BTreeMap::new(),
            transcript_type: IDkgTranscriptType::Masked(IDkgMaskedTranscriptOrigin::Random),
            algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
            internal_transcript_raw: vec![],
        };

        let fake_kappa_times_lambda = IDkgTranscript {
            transcript_id: dummy_idkg_transcript_id_for_tests(40),
            receivers: IDkgReceivers::new(nodes.clone()).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: BTreeMap::new(),
            transcript_type: IDkgTranscriptType::Masked(
                IDkgMaskedTranscriptOrigin::UnmaskedTimesMasked(kappa_id, lambda_id),
            ),
            algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
            internal_transcript_raw: vec![],
        };

        let fake_key = IDkgTranscript {
            transcript_id: key_id,
            receivers: IDkgReceivers::new(nodes.clone()).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: BTreeMap::new(),
            transcript_type: IDkgTranscriptType::Unmasked(
                IDkgUnmaskedTranscriptOrigin::ReshareMasked(dummy_idkg_transcript_id_for_tests(50)),
            ),
            algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
            internal_transcript_raw: vec![],
        };

        let fake_key_times_lambda = IDkgTranscript {
            transcript_id: dummy_idkg_transcript_id_for_tests(50),
            receivers: IDkgReceivers::new(nodes).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: BTreeMap::new(),
            transcript_type: IDkgTranscriptType::Masked(
                IDkgMaskedTranscriptOrigin::UnmaskedTimesMasked(key_id, lambda_id),
            ),
            algorithm_id: AlgorithmId::ThresholdEcdsaSecp256k1,
            internal_transcript_raw: vec![],
        };

        let presig_quadruple = PreSignatureQuadruple::new(
            fake_kappa,
            fake_lambda,
            fake_kappa_times_lambda,
            fake_key_times_lambda,
        )
        .unwrap();

        (fake_key, presig_quadruple)
    };

    let derivation_path = ExtendedDerivationPath {
        caller,
        derivation_path: vec![],
    };
    ThresholdEcdsaSigInputs::new(
        &derivation_path,
        &[],
        Randomness::from([0_u8; 32]),
        fake_presig_quadruple,
        fake_key,
    )
    .expect("failed to create signature inputs")
}

pub fn mock_dealings(
    transcript_id: IDkgTranscriptId,
    dealers: &BTreeSet<NodeId>,
) -> Vec<SignedIDkgDealing> {
    let mut dealings = Vec::new();
    for node_id in dealers {
        let signed_dealing = SignedIDkgDealing {
            content: IDkgDealing {
                transcript_id,
                internal_dealing_raw: format!("Dummy raw dealing for dealer {}", node_id)
                    .into_bytes(),
            },
            signature: BasicSignature {
                signature: BasicSigOf::new(BasicSig(vec![])),
                signer: *node_id,
            },
        };
        dealings.push(signed_dealing);
    }
    dealings
}

#[derive(Default)]
pub struct CryptoReturningOk {
    // Here we store the ids of all transcripts, which were loaded by the crypto components.
    pub loaded_transcripts: std::sync::RwLock<BTreeSet<NiDkgId>>,
    // Here we keep track of all transcripts ids asked to be retained.
    pub retained_transcripts: std::sync::RwLock<Vec<HashSet<NiDkgId>>>,
}

impl<T: Signable> BasicSigner<T> for CryptoReturningOk {
    fn sign_basic(
        &self,
        _message: &T,
        _signer: NodeId,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<BasicSigOf<T>> {
        Ok(BasicSigOf::new(BasicSig(vec![])))
    }
}

impl<T: Signable> BasicSigVerifier<T> for CryptoReturningOk {
    fn verify_basic_sig(
        &self,
        _signature: &BasicSigOf<T>,
        _message: &T,
        _signer: NodeId,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }

    fn combine_basic_sig(
        &self,
        signatures: BTreeMap<NodeId, &BasicSigOf<T>>,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<BasicSignatureBatch<T>> {
        Ok(BasicSignatureBatch {
            signatures_map: signatures
                .iter()
                .map(|(key, value)| (*key, (*value).clone()))
                .collect(),
        })
    }

    fn verify_basic_sig_batch(
        &self,
        _signature: &BasicSignatureBatch<T>,
        _message: &T,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl<T: Signable> BasicSigVerifierByPublicKey<T> for CryptoReturningOk {
    fn verify_basic_sig_by_public_key(
        &self,
        _signature: &BasicSigOf<T>,
        _signed_bytes: &T,
        _public_key: &UserPublicKey,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl<T: Signable> MultiSigner<T> for CryptoReturningOk {
    fn sign_multi(
        &self,
        _message: &T,
        _signer: NodeId,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<IndividualMultiSigOf<T>> {
        Ok(IndividualMultiSigOf::new(IndividualMultiSig(vec![])))
    }
}

impl<T: Signable> MultiSigVerifier<T> for CryptoReturningOk {
    fn verify_multi_sig_individual(
        &self,
        _signature: &IndividualMultiSigOf<T>,
        _message: &T,
        _signer: NodeId,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }

    fn combine_multi_sig_individuals(
        &self,
        _signatures: BTreeMap<NodeId, IndividualMultiSigOf<T>>,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<CombinedMultiSigOf<T>> {
        Ok(CombinedMultiSigOf::new(CombinedMultiSig(vec![])))
    }

    fn verify_multi_sig_combined(
        &self,
        _signature: &CombinedMultiSigOf<T>,
        _message: &T,
        _signers: BTreeSet<NodeId>,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl<T: Signable> ThresholdSigner<T> for CryptoReturningOk {
    fn sign_threshold(&self, _message: &T, _dkg_id: DkgId) -> CryptoResult<ThresholdSigShareOf<T>> {
        Ok(ThresholdSigShareOf::new(ThresholdSigShare(vec![])))
    }
}

impl<T: Signable> ThresholdSigVerifier<T> for CryptoReturningOk {
    fn verify_threshold_sig_share(
        &self,
        _signature: &ThresholdSigShareOf<T>,
        _message: &T,
        _dkg_id: DkgId,
        _signer: NodeId,
    ) -> CryptoResult<()> {
        Ok(())
    }

    fn combine_threshold_sig_shares(
        &self,
        _shares: BTreeMap<NodeId, ThresholdSigShareOf<T>>,
        _dkg_id: DkgId,
    ) -> CryptoResult<CombinedThresholdSigOf<T>> {
        Ok(CombinedThresholdSigOf::new(CombinedThresholdSig(vec![])))
    }

    fn verify_threshold_sig_combined(
        &self,
        _signature: &CombinedThresholdSigOf<T>,
        _message: &T,
        _dkg_id: DkgId,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl<T: Signable> ThresholdSigVerifierByPublicKey<T> for CryptoReturningOk {
    fn verify_combined_threshold_sig_by_public_key(
        &self,
        _signature: &CombinedThresholdSigOf<T>,
        _message: &T,
        _subnet_id: SubnetId,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl<T: Signable> CanisterSigVerifier<T> for CryptoReturningOk {
    fn verify_canister_sig(
        &self,
        _signature: &CanisterSigOf<T>,
        _signed_bytes: &T,
        _public_key: &UserPublicKey,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<()> {
        Ok(())
    }
}

impl NiDkgAlgorithm for CryptoReturningOk {
    fn create_dealing(&self, _config: &NiDkgConfig) -> Result<NiDkgDealing, DkgCreateDealingError> {
        Ok(empty_ni_dkg_dealing())
    }

    fn verify_dealing(
        &self,
        _config: &NiDkgConfig,
        _dealer: NodeId,
        _dealing: &NiDkgDealing,
    ) -> Result<(), DkgVerifyDealingError> {
        Ok(())
    }

    fn create_transcript(
        &self,
        config: &NiDkgConfig,
        _verified_dealings: &BTreeMap<NodeId, NiDkgDealing>,
    ) -> Result<NiDkgTranscript, DkgCreateTranscriptError> {
        let mut transcript = NiDkgTranscript::dummy_transcript_for_tests_with_params(
            config.receivers().get().clone().into_iter().collect(),
            config.dkg_id().dkg_tag,
            config.threshold().get().get() as u32,
            config.registry_version().get(),
        );
        transcript.dkg_id = config.dkg_id();
        Ok(transcript)
    }

    fn load_transcript(
        &self,
        transcript: &NiDkgTranscript,
    ) -> Result<LoadTranscriptResult, DkgLoadTranscriptError> {
        self.loaded_transcripts
            .write()
            .unwrap()
            .insert(transcript.dkg_id);
        Ok(LoadTranscriptResult::SigningKeyAvailable)
    }

    fn retain_only_active_keys(
        &self,
        transcripts: HashSet<NiDkgTranscript>,
    ) -> Result<(), DkgKeyRemovalError> {
        self.retained_transcripts
            .write()
            .unwrap()
            .push(transcripts.iter().map(|t| t.dkg_id).collect());
        Ok(())
    }
}

impl KeyManager for CryptoReturningOk {
    fn check_keys_with_registry(
        &self,
        _registry_version: RegistryVersion,
    ) -> CryptoResult<PublicKeyRegistrationStatus> {
        Ok(PublicKeyRegistrationStatus::AllKeysRegistered)
    }

    fn node_public_keys(&self) -> NodePublicKeys {
        unimplemented!()
    }
}

impl IDkgProtocol for CryptoReturningOk {
    fn create_dealing(
        &self,
        params: &IDkgTranscriptParams,
    ) -> Result<IDkgDealing, IDkgCreateDealingError> {
        let dealing = IDkgDealing {
            transcript_id: params.transcript_id(),
            internal_dealing_raw: vec![],
        };
        Ok(dealing)
    }

    fn verify_dealing_public(
        &self,
        _params: &IDkgTranscriptParams,
        _dealer_id: NodeId,
        _dealing: &IDkgDealing,
    ) -> Result<(), IDkgVerifyDealingPublicError> {
        Ok(())
    }

    fn verify_dealing_private(
        &self,
        _params: &IDkgTranscriptParams,
        _dealer_id: NodeId,
        _dealing: &IDkgDealing,
    ) -> Result<(), IDkgVerifyDealingPrivateError> {
        Ok(())
    }

    fn create_transcript(
        &self,
        params: &IDkgTranscriptParams,
        verified_dealings: &BTreeMap<NodeId, BatchSignedIDkgDealing>,
    ) -> Result<IDkgTranscript, IDkgCreateTranscriptError> {
        let mut receivers = BTreeSet::new();
        receivers.insert(node_test_id(0));

        let dealings_by_index = verified_dealings
            .iter()
            .map(|(id, d)| (params.dealers().position(*id).expect("mock"), d.clone()))
            .collect();

        Ok(IDkgTranscript {
            transcript_id: dummy_idkg_transcript_id_for_tests(0),
            receivers: IDkgReceivers::new(receivers).unwrap(),
            registry_version: RegistryVersion::from(1),
            verified_dealings: dealings_by_index,
            transcript_type: IDkgTranscriptType::Masked(IDkgMaskedTranscriptOrigin::Random),
            algorithm_id: AlgorithmId::Placeholder,
            internal_transcript_raw: vec![],
        })
    }

    // Verification all multi-sig on the various dealings in the transcript.
    fn verify_transcript(
        &self,
        _params: &IDkgTranscriptParams,
        _transcript: &IDkgTranscript,
    ) -> Result<(), IDkgVerifyTranscriptError> {
        Ok(())
    }

    fn load_transcript(
        &self,
        _transcript: &IDkgTranscript,
    ) -> Result<Vec<IDkgComplaint>, IDkgLoadTranscriptError> {
        Ok(vec![])
    }

    fn verify_complaint(
        &self,
        _transcript: &IDkgTranscript,
        _complainer_id: NodeId,
        _complaint: &IDkgComplaint,
    ) -> Result<(), IDkgVerifyComplaintError> {
        Ok(())
    }

    fn open_transcript(
        &self,
        _transcript: &IDkgTranscript,
        _complainer_id: NodeId,
        complaint: &IDkgComplaint,
    ) -> Result<IDkgOpening, IDkgOpenTranscriptError> {
        Ok(dummy_idkg_opening_for_tests(complaint))
    }

    fn verify_opening(
        &self,
        _transcript: &IDkgTranscript,
        _opener: NodeId,
        _opening: &IDkgOpening,
        _complaint: &IDkgComplaint,
    ) -> Result<(), IDkgVerifyOpeningError> {
        Ok(())
    }

    fn load_transcript_with_openings(
        &self,
        _transcript: &IDkgTranscript,
        _openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
    ) -> Result<(), IDkgLoadTranscriptError> {
        Ok(())
    }

    fn retain_active_transcripts(
        &self,
        _active_transcripts: &HashSet<IDkgTranscript>,
    ) -> Result<(), IDkgRetainThresholdKeysError> {
        Ok(())
    }
}

impl ThresholdEcdsaSigner for CryptoReturningOk {
    fn sign_share(
        &self,
        _inputs: &ThresholdEcdsaSigInputs,
    ) -> Result<ThresholdEcdsaSigShare, ThresholdEcdsaSignShareError> {
        Ok(ThresholdEcdsaSigShare {
            sig_share_raw: vec![],
        })
    }
}

impl ThresholdEcdsaSigVerifier for CryptoReturningOk {
    fn verify_sig_share(
        &self,
        _signer: NodeId,
        _inputs: &ThresholdEcdsaSigInputs,
        _share: &ThresholdEcdsaSigShare,
    ) -> Result<(), ThresholdEcdsaVerifySigShareError> {
        Ok(())
    }

    fn combine_sig_shares(
        &self,
        _inputs: &ThresholdEcdsaSigInputs,
        _shares: &BTreeMap<NodeId, ThresholdEcdsaSigShare>,
    ) -> Result<ThresholdEcdsaCombinedSignature, ThresholdEcdsaCombineSigSharesError> {
        Ok(ThresholdEcdsaCombinedSignature { signature: vec![] })
    }

    fn verify_combined_sig(
        &self,
        _inputs: &ThresholdEcdsaSigInputs,
        _signature: &ThresholdEcdsaCombinedSignature,
    ) -> Result<(), ThresholdEcdsaVerifyCombinedSignatureError> {
        Ok(())
    }
}

pub fn mock_random_number_generator() -> Box<dyn RngCore> {
    Box::new(StdRng::from_seed([0u8; 32]))
}
