use crate::sign::log_err;
use crate::CryptoComponentFatClient;
use ic_crypto_internal_csp::CryptoServiceProvider;
use ic_interfaces::crypto::IDkgProtocol;
use ic_logger::{debug, new_logger};
use ic_types::crypto::canister_threshold_sig::error::{
    IDkgCreateDealingError, IDkgCreateTranscriptError, IDkgLoadTranscriptError,
    IDkgOpenTranscriptError, IDkgRetainThresholdKeysError, IDkgVerifyComplaintError,
    IDkgVerifyDealingPrivateError, IDkgVerifyDealingPublicError, IDkgVerifyOpeningError,
    IDkgVerifyTranscriptError,
};
use ic_types::crypto::canister_threshold_sig::idkg::{
    BatchSignedIDkgDealing, IDkgComplaint, IDkgDealing, IDkgOpening, IDkgTranscript,
    IDkgTranscriptParams,
};
use ic_types::NodeId;
use std::collections::{BTreeMap, HashSet};

mod complaint;
mod dealing;
mod retain_active_keys;
mod transcript;
mod utils;

use ic_crypto_internal_logmon::metrics::MetricsDomain;
pub use utils::{
    get_mega_pubkey, mega_public_key_from_proto, MEGaPublicKeyFromProtoError,
    MegaKeyFromRegistryError,
};

/// Currently, these are implemented with noop stubs,
/// while the true implementation is in progress.
impl<C: CryptoServiceProvider> IDkgProtocol for CryptoComponentFatClient<C> {
    fn create_dealing(
        &self,
        params: &IDkgTranscriptParams,
    ) -> Result<IDkgDealing, IDkgCreateDealingError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "create_dealing",
            crypto.registry_version => params.registry_version().get(),
            crypto.dkg_config => format!("{:?}", params),
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result =
            dealing::create_dealing(&self.csp, &self.node_id, &self.registry_client, params);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "create_dealing",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn verify_dealing_public(
        &self,
        params: &IDkgTranscriptParams,
        dealer_id: NodeId,
        dealing: &IDkgDealing,
    ) -> Result<(), IDkgVerifyDealingPublicError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "verify_dealing_public",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = dealing::verify_dealing_public(&self.csp, params, dealer_id, dealing);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "verify_dealing_public",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn verify_dealing_private(
        &self,
        params: &IDkgTranscriptParams,
        dealer_id: NodeId,
        dealing: &IDkgDealing,
    ) -> Result<(), IDkgVerifyDealingPrivateError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "verify_dealing_private",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = dealing::verify_dealing_private(
            &self.csp,
            &self.node_id,
            &self.registry_client,
            params,
            dealer_id,
            dealing,
        );
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "verify_dealing_private",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn create_transcript(
        &self,
        params: &IDkgTranscriptParams,
        dealings: &BTreeMap<NodeId, BatchSignedIDkgDealing>,
    ) -> Result<IDkgTranscript, IDkgCreateTranscriptError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "create_transcript",
            crypto.registry_version => params.registry_version().get(),
            crypto.dkg_config => format!("{:?}", params),
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result =
            transcript::create_transcript(&self.csp, &self.registry_client, params, dealings);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "create_transcript",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn verify_transcript(
        &self,
        params: &IDkgTranscriptParams,
        transcript: &IDkgTranscript,
    ) -> Result<(), IDkgVerifyTranscriptError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "verify_transcript",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result =
            transcript::verify_transcript(&self.csp, &self.registry_client, params, transcript);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "verify_transcript",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn load_transcript(
        &self,
        transcript: &IDkgTranscript,
    ) -> Result<Vec<IDkgComplaint>, IDkgLoadTranscriptError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "load_transcript",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = transcript::load_transcript(
            &self.csp,
            &self.node_id,
            &self.registry_client,
            transcript,
        );
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "load_transcript",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn verify_complaint(
        &self,
        transcript: &IDkgTranscript,
        complainer_id: NodeId,
        complaint: &IDkgComplaint,
    ) -> Result<(), IDkgVerifyComplaintError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "verify_complaint",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = complaint::verify_complaint(
            &self.csp,
            &self.registry_client,
            transcript,
            complaint,
            complainer_id,
        );
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "verify_complaint",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn open_transcript(
        &self,
        transcript: &IDkgTranscript,
        complainer_id: NodeId,
        complaint: &IDkgComplaint,
    ) -> Result<IDkgOpening, IDkgOpenTranscriptError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "open_transcript",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = transcript::open_transcript(
            &self.csp,
            &self.node_id,
            &self.registry_client,
            transcript,
            complainer_id,
            complaint,
        );
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "open_transcript",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn verify_opening(
        &self,
        transcript: &IDkgTranscript,
        opener: NodeId,
        opening: &IDkgOpening,
        complaint: &IDkgComplaint,
    ) -> Result<(), IDkgVerifyOpeningError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "verify_opening",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = transcript::verify_opening(&self.csp, transcript, opener, opening, complaint);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "verify_opening",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn load_transcript_with_openings(
        &self,
        transcript: &IDkgTranscript,
        openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
    ) -> Result<(), IDkgLoadTranscriptError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "load_transcript_with_openings",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = transcript::load_transcript_with_openings(
            &self.csp,
            &self.node_id,
            &self.registry_client,
            transcript,
            openings,
        );
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "load_transcript_with_openings",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => result.is_ok(),
            crypto.error => log_err(result.as_ref().err()),
        );
        result
    }

    fn retain_active_transcripts(
        &self,
        active_transcripts: &HashSet<IDkgTranscript>,
    ) -> Result<(), IDkgRetainThresholdKeysError> {
        let logger = new_logger!(&self.logger;
            crypto.trait_name => "IDkgProtocol",
            crypto.method_name => "retain_active_transcripts",
        );
        debug!(logger;
            crypto.description => "start",
        );
        let start_time = self.metrics.now();
        let result = retain_active_keys::retain_active_transcripts(&self.csp, active_transcripts);
        self.metrics.observe_full_duration_seconds(
            MetricsDomain::IDkgProtocol,
            "retain_active_transcripts",
            start_time,
        );
        debug!(logger;
            crypto.description => "end",
            crypto.is_ok => true,
            crypto.error => "none".to_string(),
        );
        result
    }
}
