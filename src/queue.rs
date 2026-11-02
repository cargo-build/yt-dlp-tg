//! Serial durable delivery with independent publishing and native liveness. @req DL-14

use std::{cell::Cell, future::Future, rc::Rc, time::Duration};

use crate::{
    broker::{Ack, Broker},
    cache::Cache,
    config::IggyConfig,
    error::{Error, Result},
    jobs::{Job, Request, State, Store},
};

pub struct Queue {
    store: Store,
    broker: Broker,
    stopping: Cell<bool>,
}

impl Queue {
    pub async fn connect(config: &IggyConfig, cache: &Cache) -> Result<Rc<Self>> {
        let store = Store::open(cache.shared_database(), config.max_pending).await?;
        tracing::info!(
            retained = store.len().await?,
            capacity = config.max_pending,
            "Opening durable download queue"
        );
        let broker = Broker::connect(config).await?;
        Ok(Rc::new(Self {
            store,
            broker,
            stopping: Cell::new(false),
        }))
    }

    pub async fn submit(&self, request: Request) -> Result<Option<Job>> {
        if self.stopping.get() {
            return Err(Error::Queue("queue is stopping"));
        }
        let job = self.store.submit(request).await?;
        if let Some(job) = &job {
            tracing::info!(request_id = %job.id, sequence = job.sequence, "Download request accepted");
        }
        Ok(job)
    }

    pub fn stop(&self) {
        self.stopping.set(true);
    }

    /// Finish the active handler before ending liveness and the native client. @req DL-14
    pub async fn run<F, Fut>(self: Rc<Self>, handle: F) -> Result<()>
    where
        F: Fn(Request) -> Fut,
        Fut: Future<Output = ()>,
    {
        let alive = Rc::clone(&self);
        let period = self.broker.heartbeat_interval().await;
        let keeper = compio::runtime::spawn(async move {
            loop {
                compio::time::sleep(period).await;
                if alive.broker.keepalive().await.is_err() {
                    tracing::warn!("Queue reconnect will be retried by native liveness");
                }
            }
        });
        let (published, consumed) = futures_util::future::join(
            async {
                let result = self.publish().await;
                if result.is_err() {
                    tracing::error!("Queue publisher failed; retaining durable requests");
                }
                self.stop();
                result
            },
            async {
                let result = self.consume(handle).await;
                if result.is_err() {
                    tracing::error!("Queue consumer failed; retaining unacknowledged requests");
                }
                self.stop();
                result
            },
        )
        .await;
        keeper.cancel().await;
        let closed = self.broker.shutdown().await;
        published.and(consumed).and(closed)
    }

    async fn publish(&self) -> Result<()> {
        // Replay once also repairs a crash after offset commit but before row removal.
        let mut batch = self.store.retained().await?;
        loop {
            for job in batch {
                if self.stopping.get() {
                    return Ok(());
                }
                if self.broker.publish(&job.id, &self.stopping).await? {
                    self.store.mark_published(job.id).await?;
                }
            }
            if self.stopping.get() {
                return Ok(());
            }
            compio::time::sleep(Duration::from_millis(250)).await;
            batch = self.store.pending().await?;
        }
    }

    async fn consume<F, Fut>(&self, handle: F) -> Result<()>
    where
        F: Fn(Request) -> Fut,
        Fut: Future<Output = ()>,
    {
        while !self.stopping.get() {
            let Some(message) = self.broker.poll(&self.stopping).await? else {
                compio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };
            if self.stopping.get() {
                break;
            }
            match &message.request_id {
                Some(id) => {
                    if let Some(job) = self.store.get(id.clone()).await?
                        && job.state != State::Completed
                    {
                        tracing::info!(request_id = %id, offset = message.offset(), "Processing queued download");
                        handle(job.request).await;
                        self.store.complete(id.clone()).await?;
                    }
                }
                None => {
                    tracing::warn!(offset = message.offset(), "Rejected invalid queue envelope")
                }
            }
            match self.broker.acknowledge(&message, &self.stopping).await? {
                Ack::Committed => {
                    let offset = message.offset();
                    if let Some(id) = message.request_id {
                        self.store.remove(id.clone()).await?;
                        tracing::info!(request_id = %id, offset, "Download request acknowledged");
                    }
                }
                Ack::Repoll => {}
                Ack::Stopped => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::IggyConnection,
        i18n::Language,
        media::{Format, Media},
    };

    #[compio::test]
    #[ignore = "requires tests/queue_smoke.py pinned local broker fixture"]
    async fn real_broker_replay_and_active_drain() {
        let connection = std::env::var_os("IGGY_TEST_CONNECTION_FILE")
            .map(std::path::PathBuf::from)
            .expect("fixture credentials");
        let config = IggyConfig {
            connection: IggyConnection::File(connection),
            stream: "yt_dlp_tg".into(),
            topic: "downloads".into(),
            max_pending: 256,
        };
        let directory = tempfile::tempdir().expect("state");
        let cache = Cache::open(directory.path().join("files.redb"))
            .await
            .expect("cache");
        let store = Store::open(cache.shared_database(), 256)
            .await
            .expect("store");
        let request = || Request {
            bot_id: 1000,
            user_id: 42,
            chat_id: 42,
            status_message: 1,
            language: Language::from_code(Some("en")),
            media: Media {
                title: "Fixture".into(),
                url: "https://example.com/media".into(),
            },
            format: Format::M4a,
            policy: "music-m4a-v1".into(),
        };
        let pending = store
            .submit(request())
            .await
            .expect("submit")
            .expect("capacity");
        let published = store
            .submit(request())
            .await
            .expect("submit")
            .expect("capacity");
        store
            .mark_published(published.id.clone())
            .await
            .expect("published state");
        let completed = store
            .submit(request())
            .await
            .expect("submit")
            .expect("capacity");
        store
            .complete(completed.id.clone())
            .await
            .expect("completed state");
        let broker = Broker::connect(&config).await.expect("broker");
        let stop = Cell::new(false);
        broker
            .publish(&published.id, &stop)
            .await
            .expect("first record");
        broker
            .publish(&published.id, &stop)
            .await
            .expect("duplicate record");
        broker
            .publish("00000000000000000000000000000001", &stop)
            .await
            .expect("unknown record");
        broker.shutdown().await.expect("publisher shutdown");
        let count = Rc::new(Cell::new(0));
        let queue = Queue::connect(&config, &cache).await.expect("queue");
        let observed = Rc::clone(&count);
        let running = compio::runtime::spawn(Rc::clone(&queue).run(move |_| {
            observed.set(observed.get() + 1);
            async {}
        }));
        compio::time::timeout(Duration::from_secs(30), async {
            while !store.is_empty().await.expect("count") {
                compio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("recovery completed");
        queue.stop();
        running
            .await
            .expect("runner joined")
            .expect("runner stopped");
        assert_eq!(
            count.get(),
            2,
            "pending/published once; completed/duplicate/unknown never run"
        );
        assert!(store.get(pending.id).await.expect("removed").is_none());

        store
            .submit(request())
            .await
            .expect("submit")
            .expect("capacity");
        store
            .submit(request())
            .await
            .expect("submit")
            .expect("capacity");
        let queue = Queue::connect(&config, &cache).await.expect("queue");
        let gate = Rc::new(async_lock::Semaphore::new(0));
        let active = Rc::new(Cell::new(false));
        let ended = Rc::new(Cell::new(false));
        let held = Rc::clone(&gate);
        let started = Rc::clone(&active);
        let finished = Rc::clone(&ended);
        let worker = Rc::clone(&queue);
        let running = compio::runtime::spawn(async move {
            let result = worker
                .run(move |_| {
                    started.set(true);
                    let held = Rc::clone(&held);
                    async move {
                        let _permit = held.acquire().await;
                    }
                })
                .await;
            finished.set(true);
            result
        });
        compio::time::timeout(Duration::from_secs(30), async {
            while !active.get() {
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("active work");
        queue.stop();
        compio::time::sleep(Duration::from_millis(600)).await;
        assert!(!ended.get(), "shutdown cancelled active work");
        gate.add_permits(1);
        running.await.expect("joined").expect("drained");
        assert_eq!(store.retained().await.expect("backlog").len(), 2);
        let resumed = Rc::new(Cell::new(0));
        let observed = Rc::clone(&resumed);
        let queue = Queue::connect(&config, &cache)
            .await
            .expect("restarted queue");
        let running = compio::runtime::spawn(Rc::clone(&queue).run(move |_| {
            observed.set(observed.get() + 1);
            async {}
        }));
        compio::time::timeout(Duration::from_secs(30), async {
            while !store.is_empty().await.expect("count") {
                compio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("backlog resumed");
        queue.stop();
        running.await.expect("joined").expect("stopped");
        assert_eq!(
            resumed.get(),
            1,
            "completed active work must not execute again"
        );
    }
}
