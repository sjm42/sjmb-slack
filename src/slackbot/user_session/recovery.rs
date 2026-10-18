use super::*;

impl Session {
    pub(super) async fn recover(&self, through: i64) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            for (id, channel) in self.store.recovery_channels() {
                if !lock(&self.members).contains(&id) {
                    continue;
                }
                let oldest = (channel.covered_until - OVERLAP)
                    .max(through - self.config.window_seconds as i64)
                    .max(0);
                self.store.retry_channel(&id);
                // A fixed upper bound keeps concurrent live events from moving
                // the target while a multi-page recovery is in progress.
                match self.recover_channel(&id, oldest, through).await {
                    Ok(()) => {
                        self.store.finish_recovery(&id, through);
                        self.store.save().await?;
                        info!(
                            "WS {} recovered {id} through {through}",
                            self.workspace.name
                        );
                    }
                    Err(error) if authentication_error(&error) => return Err(error),
                    Err(error) => {
                        warn!(
                            "WS {} recovery incomplete for {id}: {error:#}; checkpoint retained",
                            self.workspace.name
                        );
                        if matches!(
                            error.downcast_ref::<ApiError>(),
                            Some(ApiError::RateLimited(_))
                        ) {
                            break;
                        }
                    }
                }
            }
        }
    }

    pub(super) async fn recover_channel(
        &self,
        id: &str,
        oldest: i64,
        through: i64,
    ) -> anyhow::Result<()> {
        self.recover_pages("conversations.history", id, None, oldest, through)
            .await?;
        // This includes parents discovered in channel history during this pass.
        for thread in self.store.threads(id) {
            if let Err(error) = self
                .recover_pages("conversations.replies", id, Some(&thread), oldest, through)
                .await
            {
                if matches!(error.downcast_ref::<ApiError>(),Some(ApiError::Request { code, .. }) if code == "thread_not_found")
                {
                    warn!(
                        "WS {} thread {id}/{thread} no longer exists; removing it from recovery",
                        self.workspace.name
                    );
                    self.store.forget_thread(id, &thread);
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn recover_pages(
        &self,
        method: &str,
        id: &str,
        thread: Option<&str>,
        oldest: i64,
        through: i64,
    ) -> anyhow::Result<()> {
        let lower = oldest.to_string();
        let upper = format!("{through}.999999");
        let mut cursor = String::new();
        let mut cursors = HashSet::new();
        for _ in 0..self.config.max_pages_per_channel {
            let mut params = vec![
                ("channel", id),
                ("oldest", &lower),
                ("latest", &upper),
                ("inclusive", "true"),
                ("limit", PAGE_SIZE),
                ("cursor", &cursor),
            ];
            if let Some(thread) = thread {
                params.push(("ts", thread));
            }
            let response = self.api.recovery_call(method, &params).await?;
            let messages = response["messages"]
                .as_array()
                .context("missing history messages")?;
            ensure!(messages.len() <= 1000, "history page exceeds message limit");
            for event in messages {
                let mut event = event.clone();
                // History objects do not necessarily include a channel ID.
                let object = event.as_object_mut().context("invalid history message")?;
                object.insert("channel".into(), id.into());
                object
                    .entry("type")
                    .or_insert_with(|| Value::from("message"));
                if let Some(thread) = thread {
                    object
                        .entry("thread_ts")
                        .or_insert_with(|| Value::from(thread));
                }
                // replies can include the parent outside the requested interval.
                if let Some(seconds) = event["ts"]
                    .as_str()
                    .and_then(|ts| message_timestamp(ts).ok())
                    && (oldest..=through).contains(&seconds)
                {
                    self.process(event).await?;
                }
            }
            cursor = next_cursor(&response)?;
            if cursor.is_empty() {
                return Ok(());
            }
            ensure!(
                cursors.insert(cursor.clone()),
                "history pagination cursor repeated"
            );
        }
        bail!("recovery page limit reached; raise recovery.max_pages_per_channel if necessary")
    }
}
