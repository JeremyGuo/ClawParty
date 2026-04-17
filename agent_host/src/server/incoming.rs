use super::*;

pub(super) struct IncomingDispatcher {
    server: Arc<Server>,
    active_task_count: Arc<AtomicUsize>,
    active_task_notify: Arc<Notify>,
}

impl IncomingDispatcher {
    pub(super) fn new(server: Arc<Server>) -> Self {
        Self {
            server,
            active_task_count: Arc::new(AtomicUsize::new(0)),
            active_task_notify: Arc::new(Notify::new()),
        }
    }

    pub(super) fn has_active_workers(&self) -> bool {
        self.active_task_count.load(Ordering::SeqCst) > 0
    }

    pub(super) async fn wait_for_worker_change(&self) {
        self.active_task_notify.notified().await;
    }

    pub(super) async fn dispatch(&self, message: IncomingMessage) -> Result<()> {
        if self.try_send_fast_path_agent_selection(&message).await? {
            return Ok(());
        }

        let command_lane = incoming_command_lane(message.text.as_deref());
        if matches!(command_lane, Some(IncomingCommandLane::Immediate)) {
            self.spawn_incoming_task(message, "handle_out_of_band_command_failed");
            return Ok(());
        }

        self.spawn_incoming_task(message, "handle_incoming_failed");
        Ok(())
    }

    async fn try_send_fast_path_agent_selection(&self, message: &IncomingMessage) -> Result<bool> {
        if !self
            .server
            .allows_fast_path_agent_selection(&message.address)?
        {
            return Ok(false);
        }
        let Some(outgoing) = fast_path_agent_selection_message(
            &self.server.workdir,
            &self.server.models,
            &self.server.agent,
            message,
        ) else {
            return Ok(false);
        };
        if let Some(channel) = self.server.channels.get(&message.address.channel_id)
            && let Err(error) = channel.send(&message.address, outgoing).await
        {
            error!(
                log_stream = "channel",
                log_key = %message.address.channel_id,
                kind = "fast_path_send_failed",
                conversation_id = %message.address.conversation_id,
                error = %format!("{error:#}"),
                "failed to send fast-path model selection message"
            );
        }
        Ok(true)
    }

    fn spawn_incoming_task(&self, message: IncomingMessage, failure_kind: &'static str) {
        let server = Arc::clone(&self.server);
        let active_task_count = Arc::clone(&self.active_task_count);
        let active_task_notify = Arc::clone(&self.active_task_notify);
        active_task_count.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            if let Err(error) = server.handle_incoming(message).await {
                error!(
                    log_stream = "server",
                    kind = failure_kind,
                    error = %format!("{error:#}"),
                    "failed to handle incoming message"
                );
            }
            active_task_count.fetch_sub(1, Ordering::SeqCst);
            active_task_notify.notify_waiters();
        });
    }
}
