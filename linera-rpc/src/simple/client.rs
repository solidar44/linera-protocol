// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::future::Future;

use futures::{sink::SinkExt, stream::StreamExt};
use linera_base::{
    crypto::CryptoHash,
    data_types::{BlobContent, NetworkDescription},
    identifiers::{BlobId, ChainId},
    time::{timer, Duration},
};
use linera_chain::{
    data_types::BlockProposal,
    types::{
        ConfirmedBlockCertificate, LiteCertificate, TimeoutCertificate, ValidatedBlockCertificate,
    },
};
use linera_core::{
    data_types::{ChainInfoQuery, ChainInfoResponse},
    node::{CrossChainMessageDelivery, NodeError, NotificationStream, ValidatorNode},
};
use linera_version::VersionInfo;

use super::{codec, transport::TransportProtocol};
use crate::{
    config::ValidatorPublicNetworkPreConfig, HandleConfirmedCertificateRequest,
    HandleLiteCertRequest, HandleTimeoutCertificateRequest, HandleValidatedCertificateRequest,
    RpcMessage,
};

#[derive(Clone)]
pub struct SimpleClient {
    network: ValidatorPublicNetworkPreConfig<TransportProtocol>,
    send_timeout: Duration,
    recv_timeout: Duration,
}

impl SimpleClient {
    pub(crate) fn new(
        network: ValidatorPublicNetworkPreConfig<TransportProtocol>,
        send_timeout: Duration,
        recv_timeout: Duration,
    ) -> Self {
        Self {
            network,
            send_timeout,
            recv_timeout,
        }
    }

    async fn send_recv_internal(&self, message: RpcMessage) -> Result<RpcMessage, codec::Error> {
        let address = format!("{}:{}", self.network.host, self.network.port);
        let mut stream = self.network.protocol.connect(address).await?;
        // Send message
        timer::timeout(self.send_timeout, stream.send(message))
            .await
            .map_err(|timeout| codec::Error::IoError(timeout.into()))??;
        // Wait for reply
        timer::timeout(self.recv_timeout, stream.next())
            .await
            .map_err(|timeout| codec::Error::IoError(timeout.into()))?
            .transpose()?
            .ok_or_else(|| codec::Error::IoError(std::io::ErrorKind::UnexpectedEof.into()))
    }

    async fn query<Response>(&self, query: RpcMessage) -> Result<Response, Response::Error>
    where
        Response: TryFrom<RpcMessage>,
        Response::Error: From<codec::Error>,
    {
        self.send_recv_internal(query).await?.try_into()
    }
}

impl ValidatorNode for SimpleClient {
    type NotificationStream = NotificationStream;

    /// Initiates a new block.
    async fn handle_block_proposal(
        &self,
        proposal: BlockProposal,
    ) -> Result<ChainInfoResponse, NodeError> {
        let request = RpcMessage::BlockProposal(Box::new(proposal));
        self.query(request).await
    }

    /// Processes a lite certificate.
    async fn handle_lite_certificate(
        &self,
        certificate: LiteCertificate<'_>,
        delivery: CrossChainMessageDelivery,
    ) -> Result<ChainInfoResponse, NodeError> {
        let wait_for_outgoing_messages = delivery.wait_for_outgoing_messages();
        let request = RpcMessage::LiteCertificate(Box::new(HandleLiteCertRequest {
            certificate: certificate.cloned(),
            wait_for_outgoing_messages,
        }));
        self.query(request).await
    }

    /// Processes a validated certificate.
    async fn handle_validated_certificate(
        &self,
        certificate: ValidatedBlockCertificate,
    ) -> Result<ChainInfoResponse, NodeError> {
        let request = HandleValidatedCertificateRequest { certificate };
        let request = RpcMessage::ValidatedCertificate(Box::new(request));
        self.query(request).await
    }

    /// Processes a confirmed certificate.
    async fn handle_confirmed_certificate(
        &self,
        certificate: ConfirmedBlockCertificate,
        delivery: CrossChainMessageDelivery,
    ) -> Result<ChainInfoResponse, NodeError> {
        let wait_for_outgoing_messages = delivery.wait_for_outgoing_messages();
        let request = HandleConfirmedCertificateRequest {
            certificate,
            wait_for_outgoing_messages,
        };
        let request = RpcMessage::ConfirmedCertificate(Box::new(request));
        self.query(request).await
    }

    /// Processes a timeout certificate.
    async fn handle_timeout_certificate(
        &self,
        certificate: TimeoutCertificate,
    ) -> Result<ChainInfoResponse, NodeError> {
        let request = HandleTimeoutCertificateRequest { certificate };
        let request = RpcMessage::TimeoutCertificate(Box::new(request));
        self.query(request).await
    }

    /// Handles information queries for this chain.
    async fn handle_chain_info_query(
        &self,
        query: ChainInfoQuery,
    ) -> Result<ChainInfoResponse, NodeError> {
        let request = RpcMessage::ChainInfoQuery(Box::new(query));
        self.query(request).await
    }

    fn subscribe(
        &self,
        _chains: Vec<ChainId>,
    ) -> impl Future<Output = Result<NotificationStream, NodeError>> + Send {
        let transport = self.network.protocol.to_string();
        async { Err(NodeError::SubscriptionError { transport }) }
    }

    async fn get_version_info(&self) -> Result<VersionInfo, NodeError> {
        self.query(RpcMessage::VersionInfoQuery).await
    }

    async fn get_network_description(&self) -> Result<NetworkDescription, NodeError> {
        self.query(RpcMessage::NetworkDescriptionQuery).await
    }

    async fn upload_blob(&self, content: BlobContent) -> Result<BlobId, NodeError> {
        self.query(RpcMessage::UploadBlob(Box::new(content))).await
    }

    async fn download_blob(&self, blob_id: BlobId) -> Result<BlobContent, NodeError> {
        self.query(RpcMessage::DownloadBlob(Box::new(blob_id)))
            .await
    }

    async fn download_pending_blob(
        &self,
        chain_id: ChainId,
        blob_id: BlobId,
    ) -> Result<BlobContent, NodeError> {
        self.query(RpcMessage::DownloadPendingBlob(Box::new((
            chain_id, blob_id,
        ))))
        .await
    }

    async fn handle_pending_blob(
        &self,
        chain_id: ChainId,
        blob: BlobContent,
    ) -> Result<ChainInfoResponse, NodeError> {
        self.query(RpcMessage::HandlePendingBlob(Box::new((chain_id, blob))))
            .await
    }

    async fn download_certificate(
        &self,
        hash: CryptoHash,
    ) -> Result<ConfirmedBlockCertificate, NodeError> {
        Ok(self
            .download_certificates(vec![hash])
            .await?
            .into_iter()
            .next()
            .unwrap()) // UNWRAP: We know there is exactly one certificate, otherwise we would have an error.
    }

    async fn download_certificates(
        &self,
        hashes: Vec<CryptoHash>,
    ) -> Result<Vec<ConfirmedBlockCertificate>, NodeError> {
        let certificates = self
            .query::<Vec<ConfirmedBlockCertificate>>(RpcMessage::DownloadCertificates(
                hashes.clone(),
            ))
            .await?;

        if certificates.len() != hashes.len() {
            let missing_hashes: Vec<CryptoHash> = hashes
                .into_iter()
                .filter(|hash| !certificates.iter().any(|cert| cert.hash() == *hash))
                .collect();
            Err(NodeError::MissingCertificates(missing_hashes))
        } else {
            Ok(certificates)
        }
    }

    async fn blob_last_used_by(&self, blob_id: BlobId) -> Result<CryptoHash, NodeError> {
        self.query(RpcMessage::BlobLastUsedBy(Box::new(blob_id)))
            .await
    }

    async fn missing_blob_ids(&self, blob_ids: Vec<BlobId>) -> Result<Vec<BlobId>, NodeError> {
        self.query(RpcMessage::MissingBlobIds(blob_ids)).await
    }
}
