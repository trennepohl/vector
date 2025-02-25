use std::sync::Arc;

use azure_core::{new_http_client, HttpError};
use azure_identity::{AutoRefreshingTokenCredential, DefaultAzureCredential};
use azure_storage::prelude::*;
use azure_storage_blobs::{blob::responses::PutBlockBlobResponse, prelude::*};
use bytes::Bytes;
use futures::FutureExt;
use http::StatusCode;
use snafu::Snafu;
use vector_core::{internal_event::EventsSent, stream::DriverResponse};

use crate::{
    event::{EventFinalizers, EventStatus, Finalizable},
    sinks::{util::retries::RetryLogic, Healthcheck},
};

#[derive(Debug, Clone)]
pub struct AzureBlobRequest {
    pub blob_data: Bytes,
    pub content_encoding: Option<&'static str>,
    pub content_type: &'static str,
    pub metadata: AzureBlobMetadata,
}

impl Finalizable for AzureBlobRequest {
    fn take_finalizers(&mut self) -> EventFinalizers {
        std::mem::take(&mut self.metadata.finalizers)
    }
}

#[derive(Clone, Debug)]
pub struct AzureBlobMetadata {
    pub partition_key: String,
    pub count: usize,
    pub byte_size: usize,
    pub finalizers: EventFinalizers,
}

#[derive(Debug, Clone)]
pub struct AzureBlobRetryLogic;

impl RetryLogic for AzureBlobRetryLogic {
    type Error = HttpError;
    type Response = AzureBlobResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        match error {
            HttpError::StatusCode { status, .. } => {
                status.is_server_error() || status == &StatusCode::TOO_MANY_REQUESTS
            }
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct AzureBlobResponse {
    pub inner: PutBlockBlobResponse,
    pub count: usize,
    pub events_byte_size: usize,
}

impl DriverResponse for AzureBlobResponse {
    fn event_status(&self) -> EventStatus {
        EventStatus::Delivered
    }

    fn events_sent(&self) -> EventsSent {
        EventsSent {
            count: self.count,
            byte_size: self.events_byte_size,
            output: None,
        }
    }
}

#[derive(Debug, Snafu)]
pub enum HealthcheckError {
    #[snafu(display("Invalid connection string specified"))]
    InvalidCredentials,
    #[snafu(display("Container: {:?} not found", container))]
    UnknownContainer { container: String },
    #[snafu(display("Unknown status code: {}", status))]
    Unknown { status: StatusCode },
}

pub fn build_healthcheck(
    container_name: String,
    client: Arc<ContainerClient>,
) -> crate::Result<Healthcheck> {
    let healthcheck = async move {
        let request = client.get_properties().execute().await;

        match request {
            Ok(_) => Ok(()),
            Err(reason) => Err(match reason.downcast_ref::<HttpError>() {
                Some(HttpError::StatusCode { status, .. }) => match *status {
                    StatusCode::FORBIDDEN => HealthcheckError::InvalidCredentials.into(),
                    StatusCode::NOT_FOUND => HealthcheckError::UnknownContainer {
                        container: container_name,
                    }
                    .into(),
                    status => HealthcheckError::Unknown { status }.into(),
                },
                _ => reason,
            }),
        }
    };

    Ok(healthcheck.boxed())
}

pub fn build_client(
    connection_string: Option<String>,
    storage_account: Option<String>,
    container_name: String,
) -> crate::Result<Arc<ContainerClient>> {
    let client;
    match (connection_string, storage_account) {
        (Some(connection_string_p), None) => {
            client = StorageAccountClient::new_connection_string(
                new_http_client(),
                &connection_string_p,
            )?
            .as_storage_client()
            .as_container_client(container_name);
        }
        (None, Some(storage_account_p)) => {
            let creds = std::sync::Arc::new(DefaultAzureCredential::default());
            let auto_creds = Box::new(AutoRefreshingTokenCredential::new(creds));

            client = StorageAccountClient::new_token_credential(
                new_http_client(),
                storage_account_p,
                auto_creds,
            )
            .as_storage_client()
            .as_container_client(container_name);
        }
        (None, None) => {
            return Err("Either `connection_string` or `storage_account` has to be provided".into())
        }
        (Some(_), Some(_)) => {
            return Err(
                "`connection_string` and `storage_account` can't be provided at the same time"
                    .into(),
            )
        }
    }
    Ok(client)
}
