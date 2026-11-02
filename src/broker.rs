//! Native Iggy transport with explicit recovery and private provisioning. @req DL-7 DL-14

use std::{
    cell::Cell,
    collections::BTreeMap,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use iggy::{
    io_uring::CompioTcpClient,
    prelude::{
        Client, Consumer, ConsumerOffsetClient, ConsumerPosition, Durability, Identifier,
        IggyByteSize, IggyError, IggyExpiry, IggyMessage, MaxTopicSize, MessageClient,
        Partitioning, Permissions, PollingStrategy, StreamClient, StreamCreateOptions,
        StreamPermissions, SystemClient, TopicClient, TopicCreateOptions, TopicDetails,
        TopicPermissions, UserClient, UserCreateOptions, UserStatus,
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    config::{IggyConfig, IggyConnection},
    error::{Error, Result},
    jobs,
};

pub struct Broker {
    client: Arc<CompioTcpClient>,
    stream: Identifier,
    topic: Identifier,
    consumer: Consumer,
}

pub struct Message {
    pub request_id: Option<String>,
    position: ConsumerPosition,
}

impl Message {
    pub fn offset(&self) -> u64 {
        self.position.offset
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    Committed,
    Repoll,
    Stopped,
}

enum Recovery {
    Committed,
    Replay,
    Repoll,
    Stopped,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    request_id: String,
}

const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

impl Broker {
    /// Verify the owned durable partition before accepting queued work. @req DL-14
    pub async fn connect(config: &IggyConfig) -> Result<Self> {
        let client = connect_client(config).await?;
        let result = async {
            let stream = Identifier::named(&config.stream)
                .map_err(|_| Error::Queue("invalid stream identifier"))?;
            let topic = Identifier::named(&config.topic)
                .map_err(|_| Error::Queue("invalid topic identifier"))?;
            let details = client
                .get_topic(&stream, &topic)
                .await
                .map_err(|_| Error::Queue("cannot inspect queue topic"))?
                .ok_or(Error::Queue("queue topic has not been provisioned"))?;
            validate_topic(&details)?;
            Ok(Self {
                client: Arc::clone(&client),
                stream,
                topic,
                consumer: Consumer::default(),
            })
        }
        .await;
        if result.is_err() {
            let _closed = client.shutdown().await;
        }
        result
    }

    /// Resolve uncertain writes before allowing deliberate idempotent replay. @req DL-14
    pub async fn publish(&self, id: &str, stopping: &Cell<bool>) -> Result<bool> {
        jobs::validate_id(id)?;
        let body = serde_json::to_string(&Envelope {
            version: 1,
            request_id: id.into(),
        })?;
        let mut message = IggyMessage::from_str(&body)
            .map_err(|_| Error::Queue("cannot encode queue envelope"))?;
        message.header.id =
            u128::from_str_radix(id, 16).map_err(|_| Error::Queue("invalid request identifier"))?;
        let mut messages = [message];
        while !stopping.get() {
            match self
                .client
                .send_messages(
                    &self.stream,
                    &self.topic,
                    &Partitioning::partition_id(0),
                    &mut messages,
                )
                .await
            {
                Ok(_) => return Ok(true),
                Err(error) => match self.recover(error, stopping).await? {
                    Recovery::Committed => return Ok(true),
                    Recovery::Stopped => return Ok(false),
                    Recovery::Replay | Recovery::Repoll => {}
                },
            }
        }
        Ok(false)
    }

    /// Poll without advancing the durable consumer bookmark. @req DL-14
    pub async fn poll(&self, stopping: &Cell<bool>) -> Result<Option<Message>> {
        let polled = loop {
            if stopping.get() {
                return Ok(None);
            }
            match self
                .client
                .poll_messages(
                    &self.stream,
                    &self.topic,
                    Some(0),
                    &self.consumer,
                    &PollingStrategy::next(),
                    1,
                    false,
                )
                .await
            {
                Ok(polled) => break polled,
                Err(error) => match self.recover(error, stopping).await? {
                    Recovery::Stopped => return Ok(None),
                    _ => compio::time::sleep(Duration::from_millis(250)).await,
                },
            }
        };
        if polled.messages.len() > 1 || polled.partition_id != 0 {
            return Err(Error::Queue("unexpected queue partition reply"));
        }
        let position = ConsumerPosition {
            partition_id: 0,
            offset: 0,
            context: polled.context,
        };
        Ok(polled.messages.into_iter().next().map(|message| Message {
            request_id: decode_envelope(&message.payload, message.header.id),
            position: ConsumerPosition {
                offset: message.header.offset,
                ..position
            },
        }))
    }

    /// Retain completion until the exact polled history acknowledges it. @req DL-14
    pub async fn acknowledge(&self, message: &Message, stopping: &Cell<bool>) -> Result<Ack> {
        while !stopping.get() {
            match self
                .client
                .store_consumer_position(
                    &self.consumer,
                    &self.stream,
                    &self.topic,
                    message.position,
                )
                .await
            {
                Ok(()) => return Ok(Ack::Committed),
                Err(error) => match self.recover(error, stopping).await? {
                    Recovery::Committed => return Ok(Ack::Committed),
                    Recovery::Repoll => return Ok(Ack::Repoll),
                    Recovery::Stopped => return Ok(Ack::Stopped),
                    Recovery::Replay => {}
                },
            }
        }
        Ok(Ack::Stopped)
    }

    pub async fn heartbeat_interval(&self) -> Duration {
        self.client.heartbeat_interval().await.get_duration()
    }

    pub async fn keepalive(&self) -> Result<bool> {
        self.client.keepalive().await.map_err(|error| {
            tracing::warn!(code = error.as_code(), "Iggy liveness check failed");
            Error::Queue("broker liveness failed")
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        match self.client.shutdown().await {
            Ok(()) | Err(IggyError::ClientShutdown) => Ok(()),
            Err(_) => Err(Error::Queue("broker shutdown failed")),
        }
    }

    async fn recover(&self, error: IggyError, stopping: &Cell<bool>) -> Result<Recovery> {
        let (operation, current) = match error {
            IggyError::OperationUncertain(operation) => (operation, true),
            IggyError::LaneBlocked(operation) => (operation, false),
            IggyError::HistoryUnavailable => return Ok(Recovery::Repoll),
            IggyError::OperationNotRetained(_)
            | IggyError::RequestTooOld
            | IggyError::RequestAlreadyApplied => {
                tracing::warn!("Iggy proof unavailable; replay remains idempotent by request ID");
                return Ok(Recovery::Replay);
            }
            IggyError::Disconnected
            | IggyError::NotConnected
            | IggyError::TcpError
            | IggyError::CannotEstablishConnection
            | IggyError::TransientNotAccepted
            | IggyError::TransientNotCommitted
            | IggyError::StaleClient => {
                if !stopping.get() {
                    let _liveness = self.keepalive().await;
                    compio::time::sleep(Duration::from_secs(1)).await;
                }
                return Ok(Recovery::Replay);
            }
            IggyError::ClientShutdown if stopping.get() => return Ok(Recovery::Stopped),
            error => {
                tracing::error!(code = error.as_code(), "Iggy mutation failed definitively");
                return Err(Error::Queue("broker mutation failed"));
            }
        };
        while !stopping.get() {
            match self
                .client
                .resolve_operation(operation, Instant::now() + Duration::from_secs(5))
                .await
            {
                Ok(()) => {
                    return Ok(if current {
                        Recovery::Committed
                    } else {
                        Recovery::Replay
                    });
                }
                Err(IggyError::OperationUncertain(_)) => {
                    let _liveness = self.keepalive().await;
                    compio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(
                    IggyError::OperationNotRetained(_)
                    | IggyError::RequestTooOld
                    | IggyError::RequestAlreadyApplied,
                ) => {
                    match self.client.abandon_operation(operation).await {
                        Ok(()) | Err(IggyError::OperationNotRetained(_)) => {}
                        Err(_) => {
                            return Err(Error::Queue("cannot retire uncertain broker operation"));
                        }
                    }
                    tracing::warn!(
                        "Iggy session proof lost; replay protected by durable request ID"
                    );
                    return Ok(Recovery::Replay);
                }
                Err(IggyError::HistoryUnavailable) => return Ok(Recovery::Repoll),
                Err(error) => {
                    tracing::error!(code = error.as_code(), "Iggy operation resolution failed");
                    return Err(Error::Queue("broker operation resolution failed"));
                }
            }
        }
        Ok(Recovery::Stopped)
    }
}

fn decode_envelope(bytes: &[u8], message_id: u128) -> Option<String> {
    if bytes.len() > 256 {
        return None;
    }
    let envelope = serde_json::from_slice::<Envelope>(bytes).ok()?;
    if envelope.version != 1
        || jobs::validate_id(&envelope.request_id).is_err()
        || u128::from_str_radix(&envelope.request_id, 16).ok()? != message_id
    {
        return None;
    }
    Some(envelope.request_id)
}

fn validate_topic(topic: &TopicDetails) -> Result<()> {
    let options = topic.resolved_options();
    if topic.partitions_count != 1
        || options.runtime.durability != Durability::Persisted
        || options.runtime.consumer_offset_durability != Durability::Persisted
        || options.message_expiry != IggyExpiry::NeverExpire
        || options.max_topic_size != MaxTopicSize::Unlimited
        || options.runtime.segment_size != Some(IggyByteSize::from(SEGMENT_BYTES))
        || options.runtime.preallocate_segments != Some(false)
    {
        return Err(Error::Queue(
            "queue topic differs from the persisted non-trimming small-segment profile",
        ));
    }
    Ok(())
}

async fn connect_client(config: &IggyConfig) -> Result<Arc<CompioTcpClient>> {
    let connection = config.connection_string().await?;
    let client = Arc::new(
        CompioTcpClient::from_connection_string(&connection)
            .map_err(|_| Error::Queue("invalid native broker connection"))?,
    );
    if client.connect().await.is_err() || client.ping().await.is_err() {
        let _closed = client.shutdown().await;
        return Err(Error::Queue("broker authentication/connection failed"));
    }
    Ok(client)
}

/// Provision owned resources with admin credentials, then verify the restricted user. @req DL-14
pub async fn provision(config: &IggyConfig, app_file: PathBuf) -> Result<()> {
    let client = connect_client(config).await?;
    let keeper = Arc::clone(&client);
    let period = client.heartbeat_interval().await.get_duration();
    let alive = compio::runtime::spawn(async move {
        loop {
            compio::time::sleep(period).await;
            if keeper.keepalive().await.is_err() {
                tracing::warn!("Provisioning liveness failed");
            }
        }
    });
    let result = provision_resources(&client, config, app_file).await;
    alive.cancel().await;
    let closed = client
        .shutdown()
        .await
        .map_err(|_| Error::Queue("provisioning shutdown failed"));
    result.and(closed)
}

async fn setup_result<T>(
    client: &CompioTcpClient,
    result: std::result::Result<T, IggyError>,
) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(IggyError::OperationUncertain(operation)) => client
            .resolve_operation(operation, Instant::now() + Duration::from_secs(30))
            .await
            .map_err(|_| {
                Error::Queue("provisioning outcome uncertain; inspect owned resources before retry")
            }),
        Err(error) => {
            tracing::error!(code = error.as_code(), "Broker provisioning failed");
            Err(Error::Queue("broker provisioning failed"))
        }
    }
}

async fn provision_resources(
    client: &CompioTcpClient,
    config: &IggyConfig,
    app_file: PathBuf,
) -> Result<()> {
    let stream_name =
        Identifier::named(&config.stream).map_err(|_| Error::Queue("invalid stream"))?;
    if client
        .get_stream(&stream_name)
        .await
        .map_err(|_| Error::Queue("cannot inspect stream"))?
        .is_none()
    {
        setup_result(
            client,
            client
                .create_stream(&config.stream, &StreamCreateOptions::default())
                .await,
        )
        .await?;
    }
    let stream = client
        .get_stream(&stream_name)
        .await
        .map_err(|_| Error::Queue("cannot verify stream"))?
        .ok_or(Error::Queue("stream absent after setup"))?;
    let stream_id =
        Identifier::numeric(stream.id).map_err(|_| Error::Queue("invalid stream ID"))?;
    let topic_name = Identifier::named(&config.topic).map_err(|_| Error::Queue("invalid topic"))?;
    if client
        .get_topic(&stream_id, &topic_name)
        .await
        .map_err(|_| Error::Queue("cannot inspect topic"))?
        .is_none()
    {
        let options = TopicCreateOptions {
            partitions_count: Some(1),
            segment_size: Some(IggyByteSize::from(SEGMENT_BYTES)),
            durability: Durability::Persisted,
            consumer_offset_durability: Durability::Persisted,
            message_expiry: Some(IggyExpiry::NeverExpire),
            max_topic_size: Some(MaxTopicSize::Unlimited),
            preallocate_segments: Some(false),
            ..Default::default()
        };
        setup_result(
            client,
            client
                .create_topic(&stream_id, &config.topic, &options)
                .await,
        )
        .await?;
    }
    let topic = client
        .get_topic(&stream_id, &topic_name)
        .await
        .map_err(|_| Error::Queue("cannot verify topic"))?
        .ok_or(Error::Queue("topic absent after setup"))?;
    validate_topic(&topic)?;
    let app = IggyConfig {
        connection: IggyConnection::File(app_file),
        ..config.clone()
    };
    let connection = app.connection_string().await?;
    let uri = url::Url::parse(&connection).map_err(|_| Error::Queue("invalid app credentials"))?;
    let username = percent_encoding::percent_decode_str(uri.username())
        .decode_utf8()
        .map_err(|_| Error::Queue("invalid app username"))?
        .into_owned();
    let password = percent_encoding::percent_decode_str(
        uri.password().ok_or(Error::Queue("app password missing"))?,
    )
    .decode_utf8()
    .map_err(|_| Error::Queue("invalid app password"))?
    .into_owned();
    let stream_key =
        usize::try_from(stream.id).map_err(|_| Error::Queue("invalid stream grant"))?;
    let topic_key = usize::try_from(topic.id).map_err(|_| Error::Queue("invalid topic grant"))?;
    let permissions = Permissions {
        streams: Some(BTreeMap::from([(
            stream_key,
            StreamPermissions {
                read_stream: true,
                topics: Some(BTreeMap::from([(
                    topic_key,
                    TopicPermissions {
                        read_topic: true,
                        poll_messages: true,
                        send_messages: true,
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        )])),
        ..Default::default()
    };
    let user_id = Identifier::named(&username).map_err(|_| Error::Queue("invalid app account"))?;
    match client
        .get_user(&user_id)
        .await
        .map_err(|_| Error::Queue("cannot inspect app account"))?
    {
        Some(user)
            if user.status == UserStatus::Active
                && user.permissions.as_ref() == Some(&permissions) => {}
        Some(_) => {
            return Err(Error::Queue(
                "existing app account permissions/status differ",
            ));
        }
        None => {
            setup_result(
                client,
                client
                    .create_user(
                        &username,
                        &password,
                        UserStatus::Active,
                        Some(permissions),
                        &UserCreateOptions::default(),
                    )
                    .await,
            )
            .await?
        }
    }
    let probe = Broker::connect(&app).await?;
    probe.shutdown().await?;
    println!("Iggy resources and restricted application connection verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_validate_schema_identifier_and_header() {
        let id = "00000000000000000000000000000001";
        let valid = serde_json::to_vec(&Envelope {
            version: 1,
            request_id: id.into(),
        })
        .expect("envelope");
        assert_eq!(decode_envelope(&valid, 1).as_deref(), Some(id));
        assert!(decode_envelope(&valid, 2).is_none());
        assert!(decode_envelope(br#"{"version":2,"request_id":"bad"}"#, 1).is_none());
        assert!(decode_envelope(br#"{"version":1,"request_id":"00000000000000000000000000000001","instruction":"execute"}"#, 1).is_none());
        assert!(decode_envelope(&vec![b'x'; 257], 1).is_none());
    }
}
