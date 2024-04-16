// bin/sjmb_slack.rs

use clap::Parser;

use sjmb_slack::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut opts = OptsCommon::parse();
    opts.finalize()?;
    opts.start_pgm(env!("CARGO_BIN_NAME"));

    let bot = Bot::new(&opts).await?;
    bot.run().await?;

    Ok(())
}

// EOF
