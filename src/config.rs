// config.rs

use std::env;

use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

use crate::*;

#[derive(Debug, Clone, Parser)]
pub struct OptsCommon {
    #[arg(short, long)]
    pub verbose: bool,
    #[arg(short, long)]
    pub debug: bool,
    #[arg(short, long)]
    pub trace: bool,

    #[arg(short, long, default_value = "$HOME/sjmb_slack/config/sjmb_slack.json")]
    pub bot_config: String,
}

impl OptsCommon {
    pub fn finalize(&mut self) -> anyhow::Result<()> {
        self.bot_config = shellexpand::full(&self.bot_config)?.into_owned();
        Ok(())
    }
    pub fn get_loglevel(&self) -> Level {
        if self.trace {
            Level::TRACE
        } else if self.debug {
            Level::DEBUG
        } else if self.verbose {
            Level::INFO
        } else {
            Level::ERROR
        }
    }
    fn log_filter(&self) -> Targets {
        let level = self.get_loglevel();
        Targets::new()
            .with_default(level.min(Level::INFO))
            .with_target(env!("CARGO_CRATE_NAME"), level)
    }

    pub fn start_pgm(&self, name: &str) -> anyhow::Result<()> {
        tracing_subscriber::fmt()
            .with_max_level(self.get_loglevel())
            .with_target(false)
            .finish()
            .with(self.log_filter())
            .try_init()
            .map_err(|error| anyhow::anyhow!("Could not initialize tracing: {error}"))?;

        info!("Starting up {name} v{}...", env!("CARGO_PKG_VERSION"));
        debug!("Git branch: {}", env!("GIT_BRANCH"));
        debug!("Git commit: {}", env!("GIT_COMMIT"));
        debug!("Source timestamp: {}", env!("SOURCE_TIMESTAMP"));
        debug!("Compiler version: {}", env!("RUSTC_VERSION"));
        install_crypto_provider()?;
        Ok(())
    }
}

pub(crate) fn install_crypto_provider() -> anyhow::Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Another caller can install the provider concurrently; either result
        // is fine as long as a process provider is available afterwards.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    anyhow::ensure!(
        rustls::crypto::CryptoProvider::get_default().is_some(),
        "Could not install rustls crypto provider"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_trace_are_scoped_to_the_application() {
        for (args, level) in [
            (vec!["sjmb_slack"], Level::ERROR),
            (vec!["sjmb_slack", "-v"], Level::INFO),
            (vec!["sjmb_slack", "-d"], Level::DEBUG),
            (vec!["sjmb_slack", "-t"], Level::TRACE),
        ] {
            let opts = OptsCommon::parse_from(args);
            assert_eq!(opts.get_loglevel(), level);
            let filter = opts.log_filter();
            assert_eq!(filter.default_level(), Some(level.min(Level::INFO).into()));
            assert!(
                filter
                    .iter()
                    .any(|(target, level)| target == "sjmb_slack" && level == opts.get_loglevel())
            );
        }
    }
}
