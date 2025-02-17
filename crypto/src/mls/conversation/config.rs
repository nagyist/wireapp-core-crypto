// Wire
// Copyright (C) 2022 Wire Swiss GmbH

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see http://www.gnu.org/licenses/.

//! Conversation configuration.
//!
//! Either use [MlsConversationConfiguration] when creating a conversation or [MlsCustomConfiguration]
//! when joining one by Welcome or external commit

use mls_crypto_provider::MlsCryptoProvider;
use openmls::prelude::{
    Capabilities, Credential, CredentialType, ExternalSender, OpenMlsSignaturePublicKey, ProtocolVersion,
    RequiredCapabilitiesExtension, SenderRatchetConfiguration, WireFormatPolicy, PURE_CIPHERTEXT_WIRE_FORMAT_POLICY,
    PURE_PLAINTEXT_WIRE_FORMAT_POLICY,
};
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::types::{Ciphersuite, SignatureScheme};
use openmls_traits::OpenMlsCryptoProvider;
use serde::{Deserialize, Serialize};
use wire_e2e_identity::prelude::parse_json_jwk;

use crate::prelude::{CryptoResult, E2eIdentityError, MlsCentral, MlsCiphersuite};
use crate::MlsError;

/// Sets the config in OpenMls for the oldest possible epoch(past current) that a message can be decrypted
pub(crate) const MAX_PAST_EPOCHS: usize = 3;

/// Window for which decryption secrets are kept within an epoch. Use this with caution since this affects forward secrecy within an epoch.
/// Use this when the Delivery Service cannot guarantee application messages order
pub(crate) const OUT_OF_ORDER_TOLERANCE: u32 = 2;

/// How many application messages can be skipped. Use this when the Delivery Service can drop application messages
pub(crate) const MAXIMUM_FORWARD_DISTANCE: u32 = 1000;

impl MlsCentral {
    /// Parses supplied key from Delivery Service in order to build back an [ExternalSender]
    pub fn set_raw_external_senders(
        &self,
        cfg: &mut MlsConversationConfiguration,
        external_senders: Vec<Vec<u8>>,
    ) -> CryptoResult<()> {
        cfg.external_senders = external_senders
            .into_iter()
            .map(|key| {
                MlsConversationConfiguration::parse_external_sender(&key).or_else(|_| {
                    MlsConversationConfiguration::legacy_external_sender(
                        key,
                        cfg.ciphersuite.signature_algorithm(),
                        &self.mls_backend,
                    )
                })
            })
            .collect::<CryptoResult<_>>()?;
        Ok(())
    }
}

/// The configuration parameters for a group/conversation
#[derive(Debug, Clone, Default)]
pub struct MlsConversationConfiguration {
    /// The `OpenMls` Ciphersuite used in the group
    pub ciphersuite: MlsCiphersuite,
    /// Delivery service public signature key and credential
    pub external_senders: Vec<ExternalSender>,
    /// Implementation specific configuration
    pub custom: MlsCustomConfiguration,
}

impl MlsConversationConfiguration {
    // TODO: pending a long term solution with a real certificate
    const WIRE_SERVER_IDENTITY: &'static str = "wire-server";

    const PADDING_SIZE: usize = 128;

    /// Default protocol
    pub(crate) const DEFAULT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::Mls10;

    /// List all until further notice
    pub(crate) const DEFAULT_SUPPORTED_CREDENTIALS: &'static [CredentialType] =
        &[CredentialType::Basic, CredentialType::X509];

    /// Conservative sensible defaults
    pub(crate) const DEFAULT_SUPPORTED_CIPHERSUITES: &'static [Ciphersuite] = &[
        Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519,
        Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256,
        Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519,
        Ciphersuite::MLS_256_DHKEMP384_AES256GCM_SHA384_P384,
        Ciphersuite::MLS_256_DHKEMP521_AES256GCM_SHA512_P521,
        Ciphersuite::MLS_128_X25519KYBER768DRAFT00_AES128GCM_SHA256_Ed25519,
    ];

    /// Not used at the moment
    const NUMBER_RESUMPTION_PSK: usize = 1;

    /// Generates an `MlsGroupConfig` from this configuration
    #[inline(always)]
    pub fn as_openmls_default_configuration(&self) -> CryptoResult<openmls::group::MlsGroupConfig> {
        let crypto_config = openmls::prelude::CryptoConfig {
            version: Self::DEFAULT_PROTOCOL_VERSION,
            ciphersuite: self.ciphersuite.into(),
        };
        Ok(openmls::group::MlsGroupConfig::builder()
            .wire_format_policy(self.custom.wire_policy.into())
            .max_past_epochs(MAX_PAST_EPOCHS)
            .padding_size(Self::PADDING_SIZE)
            .number_of_resumption_psks(Self::NUMBER_RESUMPTION_PSK)
            .leaf_capabilities(Self::default_leaf_capabilities())
            .required_capabilities(self.default_required_capabilities())
            .sender_ratchet_configuration(SenderRatchetConfiguration::new(
                self.custom.out_of_order_tolerance,
                self.custom.maximum_forward_distance,
            ))
            .use_ratchet_tree_extension(true)
            .external_senders(self.external_senders.clone())
            .crypto_config(crypto_config)
            .build())
    }

    /// Default capabilities for every generated [openmls::prelude::KeyPackage]
    pub fn default_leaf_capabilities() -> Capabilities {
        Capabilities::new(
            Some(&[Self::DEFAULT_PROTOCOL_VERSION]),
            Some(Self::DEFAULT_SUPPORTED_CIPHERSUITES),
            Some(&[]),
            Some(&[]),
            Some(Self::DEFAULT_SUPPORTED_CREDENTIALS),
        )
    }

    fn default_required_capabilities(&self) -> RequiredCapabilitiesExtension {
        RequiredCapabilitiesExtension::new(&[], &[], Self::DEFAULT_SUPPORTED_CREDENTIALS)
    }

    /// This expects a raw json serialized JWK. It works with any Signature scheme
    fn parse_external_sender(jwk: &[u8]) -> CryptoResult<ExternalSender> {
        let pk = parse_json_jwk(jwk)
            .map_err(wire_e2e_identity::prelude::E2eIdentityError::from)
            .map_err(E2eIdentityError::from)?;
        Ok(ExternalSender::new(
            pk.into(),
            Credential::new_basic(Self::WIRE_SERVER_IDENTITY.into()),
        ))
    }

    /// This supports the legacy behaviour where the server was providing the external sender public key
    /// raw. This only supports Ed25519
    // TODO: remove at some point when the backend API is not used anymore
    fn legacy_external_sender(
        key: Vec<u8>,
        signature_scheme: SignatureScheme,
        backend: &MlsCryptoProvider,
    ) -> CryptoResult<ExternalSender> {
        backend
            .crypto()
            .validate_signature_key(signature_scheme, &key[..])
            .map_err(MlsError::from)?;
        let key = OpenMlsSignaturePublicKey::new(key.into(), signature_scheme).map_err(MlsError::from)?;
        Ok(ExternalSender::new(
            key.into(),
            Credential::new_basic(Self::WIRE_SERVER_IDENTITY.into()),
        ))
    }
}

/// The configuration parameters for a group/conversation which are not handled natively by openmls
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlsCustomConfiguration {
    // TODO: Not implemented yet
    /// Duration in seconds after which we will automatically force a self_update commit
    pub key_rotation_span: Option<std::time::Duration>,
    /// Defines if handshake messages are encrypted or not
    pub wire_policy: MlsWirePolicy,
    /// Window for which decryption secrets are kept within an epoch. Use this with caution since
    /// this affects forward secrecy within an epoch. Use this when the Delivery Service cannot
    /// guarantee application messages order.
    pub out_of_order_tolerance: u32,
    /// How many application messages can be skipped. Use this when the Delivery Service can drop
    /// application messages
    pub maximum_forward_distance: u32,
}

impl Default for MlsCustomConfiguration {
    fn default() -> Self {
        Self {
            wire_policy: MlsWirePolicy::Plaintext,
            key_rotation_span: Default::default(),
            out_of_order_tolerance: OUT_OF_ORDER_TOLERANCE,
            maximum_forward_distance: MAXIMUM_FORWARD_DISTANCE,
        }
    }
}

/// Wrapper over [WireFormatPolicy](openmls::prelude::WireFormatPolicy)
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MlsWirePolicy {
    /// Handshake messages are never encrypted
    #[default]
    Plaintext = 1,
    /// Handshake messages are always encrypted
    Ciphertext = 2,
}

impl From<MlsWirePolicy> for WireFormatPolicy {
    fn from(policy: MlsWirePolicy) -> Self {
        match policy {
            MlsWirePolicy::Ciphertext => PURE_CIPHERTEXT_WIRE_FORMAT_POLICY,
            MlsWirePolicy::Plaintext => PURE_PLAINTEXT_WIRE_FORMAT_POLICY,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use openmls::prelude::ProtocolVersion;
    use openmls_traits::{
        crypto::OpenMlsCrypto,
        types::{SignatureScheme, VerifiableCiphersuite},
        OpenMlsCryptoProvider,
    };
    use wasm_bindgen_test::*;
    use wire_e2e_identity::prelude::JwsAlgorithm;

    use crate::{prelude::MlsConversationConfiguration, test_utils::*};

    wasm_bindgen_test_configure!(run_in_browser);

    #[cfg_attr(not(target_family = "wasm"), async_std::test)]
    #[wasm_bindgen_test]
    pub async fn group_should_have_required_capabilities() {
        let case = TestCase::default();
        run_test_with_client_ids(case.clone(), ["alice"], move |[mut cc]| {
            Box::pin(async move {
                let id = conversation_id();
                cc.mls_central
                    .new_conversation(&id, case.credential_type, case.cfg.clone())
                    .await
                    .unwrap();
                let conv = cc.mls_central.get_conversation(&id).await.unwrap();
                let group = conv.read().await;

                let capabilities = group.group.group_context_extensions().required_capabilities().unwrap();

                // see https://www.rfc-editor.org/rfc/rfc9420.html#section-11.1
                assert!(capabilities.extension_types().is_empty());
                assert!(capabilities.proposal_types().is_empty());
                assert_eq!(
                    capabilities.credential_types(),
                    MlsConversationConfiguration::DEFAULT_SUPPORTED_CREDENTIALS
                );
            })
        })
        .await
    }

    #[apply(all_cred_cipher)]
    #[wasm_bindgen_test]
    pub async fn creator_leaf_node_should_have_default_capabilities(case: TestCase) {
        run_test_with_client_ids(case.clone(), ["alice"], move |[mut cc]| {
            Box::pin(async move {
                let id = conversation_id();
                cc.mls_central
                    .new_conversation(&id, case.credential_type, case.cfg.clone())
                    .await
                    .unwrap();
                let conv = cc.mls_central.get_conversation(&id).await.unwrap();
                let group = conv.read().await;

                // verifying https://www.rfc-editor.org/rfc/rfc9420.html#section-7.2
                let creator_capabilities = group.group.own_leaf().unwrap().capabilities();

                // https://www.rfc-editor.org/rfc/rfc9420.html#section-7.2-5.1.1
                // ProtocolVersion must be the default one
                assert_eq!(creator_capabilities.versions(), &[ProtocolVersion::Mls10]);

                // To prevent downgrade attacks, Ciphersuite MUST ONLY contain the current one
                assert_eq!(
                    creator_capabilities.ciphersuites().to_vec(),
                    MlsConversationConfiguration::DEFAULT_SUPPORTED_CIPHERSUITES
                        .iter()
                        .map(|c| VerifiableCiphersuite::from(*c))
                        .collect::<Vec<_>>()
                );

                // Proposals MUST be empty since we support all the default ones
                assert!(creator_capabilities.proposals().is_empty());

                // Extensions MUST only contain non-default extension (i.e. empty for now)
                assert!(creator_capabilities.extensions().is_empty(),);

                // To prevent downgrade attacks, Credentials should just contain the current
                assert_eq!(
                    creator_capabilities.credentials(),
                    MlsConversationConfiguration::DEFAULT_SUPPORTED_CREDENTIALS
                );
            })
        })
        .await
    }

    #[apply(all_cred_cipher)]
    #[wasm_bindgen_test]
    pub async fn should_support_raw_external_sender(case: TestCase) {
        run_test_with_client_ids(case.clone(), ["alice"], move |[cc]| {
            Box::pin(async move {
                let (_sk, pk) = cc
                    .mls_central
                    .mls_backend
                    .crypto()
                    .signature_key_gen(case.signature_scheme())
                    .unwrap();

                assert!(cc
                    .mls_central
                    .set_raw_external_senders(&mut case.cfg.clone(), vec![pk])
                    .is_ok());
            })
        })
        .await
    }

    #[apply(all_cred_cipher)]
    #[wasm_bindgen_test]
    pub async fn should_support_jwk_external_sender(case: TestCase) {
        run_test_with_client_ids(case.clone(), ["alice"], move |[cc]| {
            Box::pin(async move {
                let sc = case.signature_scheme();

                let alg = match sc {
                    SignatureScheme::ED25519 => JwsAlgorithm::Ed25519,
                    SignatureScheme::ECDSA_SECP256R1_SHA256 => JwsAlgorithm::P256,
                    SignatureScheme::ECDSA_SECP384R1_SHA384 => JwsAlgorithm::P384,
                    SignatureScheme::ECDSA_SECP521R1_SHA512 => JwsAlgorithm::P521,
                    SignatureScheme::ED448 => unreachable!(),
                };

                let jwk = wire_e2e_identity::prelude::generate_jwk(alg);
                let _ = cc
                    .mls_central
                    .set_raw_external_senders(&mut case.cfg.clone(), vec![jwk])
                    .unwrap();
            })
        })
        .await;
    }
}
