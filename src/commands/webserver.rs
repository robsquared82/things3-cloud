use anyhow::{Result, bail};
use clap::Args;

use crate::{app::Cli, commands::Command};

#[derive(Debug, Args)]
pub struct WebserverArgs {
    /// Host interface to bind to. Ignored: the server is disabled.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// TCP port to listen on. Ignored: the server is disabled.
    #[arg(long, default_value_t = 8765)]
    pub port: u16,
}

impl Command for WebserverArgs {
    fn run_with_ctx(
        &self,
        _cli: &Cli,
        _out: &mut dyn std::io::Write,
        _ctx: &mut dyn crate::cmd_ctx::CmdCtx,
    ) -> Result<()> {
        bail!(
            "things3 webserver is disabled (requested {}:{}). It accepted \
             unauthenticated HTTP commands and could be asked to listen on a \
             public address. Run the things3 CLI directly.",
            self.host,
            self.port
        )
    }
}
