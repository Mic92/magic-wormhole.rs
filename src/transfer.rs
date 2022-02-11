//! Client-to-Client protocol to organize file transfers
//!
//! This gives you the actual capability to transfer files, that feature that Magic Wormhole got known and loved for.
//!
//! It is bound to an [`APPID`](APPID). Only applications using that APPID (and thus this protocol) can interoperate with
//! the original Python implementation (and other compliant implementations).
//!
//! At its core, "peer messages" are exchanged over an established wormhole connection with the other side.
//! They are used to set up a [transit] portal and to exchange a file offer/accept. Then, the file is transmitted over the transit relay.

use futures::{AsyncRead, AsyncWrite};
use serde_derive::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use std::sync::Arc;

use super::{core::WormholeError, transit, transit::Transit, AppID, Wormhole};
use log::*;
use std::{borrow::Cow, path::PathBuf};
use transit::{TransitConnectError, TransitConnector, TransitError};

mod messages;
use messages::*;
mod v1;
mod v2;

const APPID_RAW: &str = "lothar.com/wormhole/text-or-file-xfer";

/// The App ID associated with this protocol.
pub const APPID: AppID = AppID(Cow::Borrowed(APPID_RAW));

/// An [`crate::AppConfig`] with sane defaults for this protocol.
///
/// You **must not** change `id` and `rendezvous_url` to be interoperable.
/// The `app_version` can be adjusted if you want to disable some features.
pub const APP_CONFIG: crate::AppConfig<AppVersion> = crate::AppConfig::<AppVersion> {
    id: AppID(Cow::Borrowed(APPID_RAW)),
    rendezvous_url: Cow::Borrowed(crate::rendezvous::DEFAULT_RENDEZVOUS_SERVER),
    app_version: AppVersion::new(),
};

// TODO be more extensible on the JSON enum types (i.e. recognize unknown variants)

// TODO send peer errors when something went wrong (if possible)
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferError {
    #[error("Transfer was not acknowledged by peer")]
    AckError,
    #[error("Receive checksum error")]
    Checksum,
    #[error("The file contained a different amount of bytes than advertized! Sent {} bytes, but should have been {}", sent_size, file_size)]
    FileSize { sent_size: u64, file_size: u64 },
    #[error("The file(s) to send got modified during the transfer, and thus corrupted")]
    FilesystemSkew,
    // TODO be more specific
    #[error("Unsupported offer type")]
    UnsupportedOffer,
    #[error("Something went wrong on the other side: {}", _0)]
    PeerError(String),

    /// Some deserialization went wrong, we probably got some garbage
    #[error("Corrupt JSON message received")]
    ProtocolJson(
        #[from]
        #[source]
        serde_json::Error,
    ),
    #[error("Corrupt Msgpack message received")]
    ProtocolMsgpack(
        #[from]
        #[source]
        rmp_serde::decode::Error,
    ),
    /// A generic string message for "something went wrong", i.e.
    /// the server sent some bullshit message order
    #[error("Protocol error: {}", _0)]
    Protocol(Box<str>),
    #[error(
        "Unexpected message (protocol error): Expected '{}', but got: {:?}",
        _0,
        _1
    )]
    ProtocolUnexpectedMessage(Box<str>, Box<dyn std::fmt::Debug + Send + Sync>),
    #[error("Wormhole connection error")]
    Wormhole(
        #[from]
        #[source]
        WormholeError,
    ),
    #[error("Error while establishing transit connection")]
    TransitConnect(
        #[from]
        #[source]
        TransitConnectError,
    ),
    #[error("Transit error")]
    Transit(
        #[from]
        #[source]
        TransitError,
    ),
    #[error("IO error")]
    IO(
        #[from]
        #[source]
        std::io::Error,
    ),
}

impl TransferError {
    pub(self) fn unexpected_message(
        expected: impl Into<Box<str>>,
        got: impl std::fmt::Debug + Send + Sync + 'static,
    ) -> Self {
        Self::ProtocolUnexpectedMessage(expected.into(), Box::new(got))
    }
}

/**
 * The application specific version information for this protocol.
 *
 * At the moment, this always is an empty object, but this will likely change in the future.
 */
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AppVersion {
    // #[serde(default)]
// abilities: Cow<'static, [Cow<'static, str>]>,
// #[serde(default)]
// transfer_v2: Option<AppVersionTransferV2Hint>,
}

// TODO check invariants during deserialization

impl AppVersion {
    const fn new() -> Self {
        Self {
            // abilities: Cow::Borrowed([Cow::Borrowed("transfer-v1"), Cow::Borrowed("transfer-v2")]),
            // transfer_v2: Some(AppVersionTransferV2Hint::new())
        }
    }

    #[allow(dead_code)]
    fn supports_v2(&self) -> bool {
        false
        // self.abilities.contains(&"transfer-v2".into())
    }
}

impl Default for AppVersion {
    fn default() -> Self {
        Self::new()
    }
}

// #[derive(Clone, Debug, Serialize, Deserialize)]
// #[serde(rename_all = "kebab-case")]
// pub struct AppVersionTransferV2Hint {
//     supported_formats: Vec<Cow<'static, str>>,
//     transit_abilities: Vec<transit::Ability>,
// }

// impl AppVersionTransferV2Hint {
//     const fn new() -> Self {
//         Self {
//             supported_formats: vec![Cow::Borrowed("tar.zst")],
//             transit_abilities: transit::Ability::all_abilities(),
//         }
//     }
// }

// impl Default for AppVersionTransferV2Hint {
//     fn default() -> Self {
//         Self::new()
//     }
// }

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
struct TransitAck {
    pub ack: String,
    pub sha256: String,
}

impl TransitAck {
    pub fn new(msg: impl Into<String>, sha256: impl Into<String>) -> Self {
        TransitAck {
            ack: msg.into(),
            sha256: sha256.into(),
        }
    }

    #[cfg(test)]
    pub fn serialize(&self) -> String {
        json!(self).to_string()
    }

    pub fn serialize_vec(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap()
    }
}

pub async fn send_file_or_folder<N, M, H>(
    wormhole: Wormhole,
    relay_url: url::Url,
    file_path: N,
    file_name: M,
    progress_handler: H,
) -> Result<(), TransferError>
where
    N: AsRef<async_std::path::Path>,
    M: AsRef<async_std::path::Path>,
    H: FnMut(u64, u64) + 'static,
{
    use async_std::fs::File;
    let file_path = file_path.as_ref();
    let file_name = file_name.as_ref();

    let mut file = File::open(file_path).await?;
    let metadata = file.metadata().await?;
    if metadata.is_dir() {
        send_folder(wormhole, relay_url, file_path, file_name, progress_handler).await?;
    } else {
        let file_size = metadata.len();
        send_file(
            wormhole,
            relay_url,
            &mut file,
            file_name,
            file_size,
            progress_handler,
        )
        .await?;
    }
    Ok(())
}

/// Send a file to the other side
///
/// You must ensure that the Reader contains exactly as many bytes
/// as advertized in file_size.
pub async fn send_file<F, N, H>(
    wormhole: Wormhole,
    relay_url: url::Url,
    file: &mut F,
    file_name: N,
    file_size: u64,
    progress_handler: H,
) -> Result<(), TransferError>
where
    F: AsyncRead + Unpin,
    N: Into<PathBuf>,
    H: FnMut(u64, u64) + 'static,
{
    let _peer_version: AppVersion = serde_json::from_value(wormhole.peer_version.clone())?;
    let relay_hints = vec![transit::RelayHint::from_urls(None, [relay_url])];
    // if peer_version.supports_v2() && false {
    //     v2::send_file(wormhole, relay_url, file, file_name, file_size, progress_handler, peer_version).await
    // } else {
    //     log::info!("TODO");
    v1::send_file(
        wormhole,
        relay_hints,
        file,
        file_name,
        file_size,
        progress_handler,
    )
    .await
    // }
}

/// Send a folder to the other side
///
/// This isn't a proper folder transfer as per the Wormhole protocol
/// because it sends it in a way so that the receiver still has to manually
/// unpack it. But it's better than nothing
pub async fn send_folder<N, M, H>(
    wormhole: Wormhole,
    relay_url: url::Url,
    folder_path: N,
    folder_name: M,
    progress_handler: H,
) -> Result<(), TransferError>
where
    N: Into<PathBuf>,
    M: Into<PathBuf>,
    H: FnMut(u64, u64) + 'static,
{
    let relay_hints = vec![transit::RelayHint::from_urls(None, [relay_url])];
    v1::send_folder(
        wormhole,
        relay_hints,
        folder_path,
        folder_name,
        progress_handler,
    )
    .await
}

/**
 * Wait for a file offer from the other side
 *
 * This method waits for an offer message and builds up a [`ReceiveRequest`](ReceiveRequest).
 * It will also start building a TCP connection to the other side using the transit protocol.
 */
pub async fn request_file(
    mut wormhole: Wormhole,
    relay_url: url::Url,
) -> Result<ReceiveRequest, TransferError> {
    let relay_hints = vec![transit::RelayHint::from_urls(None, [relay_url])];
    let connector = transit::init(transit::Abilities::ALL_ABILITIES, None, relay_hints).await?;

    // send the transit message
    debug!("Sending transit message '{:?}", connector.our_hints());
    wormhole
        .send_json(&PeerMessage::transit(
            *connector.our_abilities(),
            (**connector.our_hints()).clone().into(),
        ))
        .await?;

    // receive transit message
    let (their_abilities, their_hints): (transit::Abilities, transit::Hints) =
        match serde_json::from_slice(&wormhole.receive().await?)? {
            PeerMessage::Transit(transit) => {
                debug!("received transit message: {:?}", transit);
                (transit.abilities_v1, transit.hints_v1.into())
            },
            PeerMessage::Error(err) => {
                bail!(TransferError::PeerError(err));
            },
            other => {
                let error = TransferError::unexpected_message("transit", other);
                let _ = wormhole
                    .send_json(&PeerMessage::Error(format!("{}", error)))
                    .await;
                bail!(error)
            },
        };

    // 3. receive file offer message from peer
    let maybe_offer = serde_json::from_slice(&wormhole.receive().await?)?;
    debug!("Received offer message '{:?}'", &maybe_offer);

    let (filename, filesize) = match maybe_offer {
        PeerMessage::Offer(offer_type) => match offer_type {
            Offer::File { filename, filesize } => (filename, filesize),
            Offer::Directory {
                mut dirname,
                zipsize,
                ..
            } => {
                dirname.set_extension("zip");
                (dirname, zipsize)
            },
            _ => bail!(TransferError::UnsupportedOffer),
        },
        PeerMessage::Error(err) => {
            bail!(TransferError::PeerError(err));
        },
        _ => {
            let error = TransferError::unexpected_message("offer", maybe_offer);
            let _ = wormhole
                .send_json(&PeerMessage::Error(format!("{}", error)))
                .await;
            bail!(error)
        },
    };

    let req = ReceiveRequest {
        wormhole,
        filename,
        filesize,
        connector,
        their_abilities,
        their_hints: Arc::new(their_hints),
    };

    Ok(req)
}

/**
 * A pending files send offer from the other side
 *
 * You *should* consume this object, either by calling [`accept`](ReceiveRequest::accept) or [`reject`](ReceiveRequest::reject).
 */
#[must_use]
pub struct ReceiveRequest {
    wormhole: Wormhole,
    connector: TransitConnector,
    /// **Security warning:** this is untrusted and unverified input
    pub filename: PathBuf,
    pub filesize: u64,
    their_abilities: transit::Abilities,
    their_hints: Arc<transit::Hints>,
}

impl ReceiveRequest {
    /**
     * Accept the file offer
     *
     * This will transfer the file and save it on disk.
     */
    pub async fn accept<F, W>(
        mut self,
        progress_handler: F,
        content_handler: &mut W,
    ) -> Result<(), TransferError>
    where
        F: FnMut(u64, u64) + 'static,
        W: AsyncWrite + Unpin,
    {
        // send file ack.
        debug!("Sending ack");
        self.wormhole
            .send_json(&PeerMessage::file_ack("ok"))
            .await?;

        let mut transit = match self
            .connector
            .follower_connect(
                self.wormhole
                    .key()
                    .derive_transit_key(self.wormhole.appid()),
                self.their_abilities.clone(),
                self.their_hints.clone(),
            )
            .await
        {
            Ok(transit) => transit,
            Err(error) => {
                let error = TransferError::TransitConnect(error);
                let _ = self
                    .wormhole
                    .send_json(&PeerMessage::Error(format!("{}", error)))
                    .await;
                return Err(error);
            },
        };

        debug!("Beginning file transfer");
        // TODO here's the right position for applying the output directory and to check for malicious (relative) file paths
        match v1::tcp_file_receive(
            &mut transit,
            self.filesize,
            progress_handler,
            content_handler,
        )
        .await
        {
            Err(TransferError::Transit(error)) => {
                let _ = self
                    .wormhole
                    .send_json(&PeerMessage::Error(format!("{}", error)))
                    .await;
                Err(TransferError::Transit(error))
            },
            other => other,
        }?;

        self.wormhole.close().await?;

        Ok(())
    }

    /**
     * Reject the file offer
     *
     * This will send an error message to the other side so that it knows the transfer failed.
     */
    pub async fn reject(mut self) -> Result<(), TransferError> {
        self.wormhole
            .send_json(&PeerMessage::error_message("transfer rejected"))
            .await?;
        self.wormhole.close().await?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_transit_ack() {
        let f1 = TransitAck::new("ok", "deadbeaf");
        assert_eq!(f1.serialize(), "{\"ack\":\"ok\",\"sha256\":\"deadbeaf\"}");
    }
}
