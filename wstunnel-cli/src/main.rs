use clap::Parser;
use std::io;
use std::str::FromStr;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;
use uuid::Uuid;
use wstunnel::LocalProtocol;
use wstunnel::config::{Client, Server};
use wstunnel::executor::DefaultTokioExecutor;
use wstunnel::ssh_client::{SshClientConfig, run_ssh_client};
use wstunnel::{run_client, run_server};

#[cfg(feature = "jemalloc")]
use tikv_jemallocator::Jemalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Use Websocket or HTTP2 protocol to tunnel {TCP,UDP} traffic
/// wsTunnelClient <---> wsTunnelServer <---> RemoteHost
#[derive(clap::Parser, Debug)]
#[command(author, version, about, verbatim_doc_comment, long_about = None)]
pub struct Wstunnel {
    #[command(subcommand)]
    commands: Commands,

    /// Disable color output in logs
    #[arg(long, global = true, verbatim_doc_comment, env = "NO_COLOR")]
    no_color: Option<String>,

    /// *WARNING* The flag does nothing, you need to set the env variable *WARNING*
    /// Control the number of threads that will be used.
    /// By default, it is equal the number of cpus
    #[arg(
        long,
        global = true,
        value_name = "INT",
        verbatim_doc_comment,
        env = "TOKIO_WORKER_THREADS"
    )]
    nb_worker_threads: Option<u32>,

    /// Control the log verbosity. i.e: TRACE, DEBUG, INFO, WARN, ERROR, OFF
    /// for more details: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#example-syntax
    #[arg(
        long,
        global = true,
        value_name = "LOG_LEVEL",
        verbatim_doc_comment,
        env = "RUST_LOG",
        default_value = "INFO"
    )]
    log_lvl: String,
}

#[derive(clap::Subcommand, Debug)]
pub enum Commands {
    Client(Box<Client>),
    Server(Box<Server>),
    Ssh(SshArgs),
}

#[derive(clap::Args, Debug)]
pub struct SshArgs {
    #[command(flatten)]
    pub client: Client,

    /// Tunnel id (uuid) to resolve server-side.
    #[arg(long, value_name = "UUID")]
    pub tunnel: Uuid,

    /// SSH username.
    #[arg(long)]
    pub user: String,

    /// Path to SSH private key (OpenSSH format).
    #[arg(long, value_name = "FILE")]
    pub key: std::path::PathBuf,

    /// Optional private key passphrase.
    #[arg(long, env = "WSTUNNEL_SSH_KEY_PASSPHRASE")]
    pub key_passphrase: Option<String>,

    /// Terminal type to request (PTY).
    #[arg(long, default_value = "xterm-256color")]
    pub term: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Wstunnel::parse();

    // Setup logging
    let mut env_filter = EnvFilter::builder().parse(&args.log_lvl).expect("Invalid log level");
    if !(args.log_lvl.contains("h2::") || args.log_lvl.contains("h2=")) {
        env_filter = env_filter.add_directive(Directive::from_str("h2::codec=off").expect("Invalid log directive"));
    }
    let logger = tracing_subscriber::fmt()
        .with_ansi(args.no_color.is_none())
        .with_env_filter(env_filter);

    // stdio tunnel captures stdio, so log to stderr (also for interactive ssh).
    match &args.commands {
        Commands::Client(args) => {
            if args.local_to_remote.iter().any(|x| {
                matches!(
                    x.local_protocol,
                    LocalProtocol::Stdio { .. } | LocalProtocol::TunnelStdio { .. }
                )
            }) {
                logger.with_writer(io::stderr).init();
            } else {
                logger.init()
            }
        }
        Commands::Ssh(_) => {
            logger.with_writer(io::stderr).init();
        }
        Commands::Server(_) => logger.init(),
    };
    if let Err(err) = fdlimit::raise_fd_limit() {
        warn!("Failed to set soft filelimit to hard file limit: {}", err)
    }

    match args.commands {
        Commands::Client(args) => {
            run_client(*args, DefaultTokioExecutor::default())
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot start wstunnel client: {err:?}");
                });
        }
        Commands::Server(args) => {
            run_server(*args, DefaultTokioExecutor::default())
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot start wstunnel server: {err:?}");
                });
        }
        Commands::Ssh(args) => {
            run_ssh_client(
                SshClientConfig {
                    client: args.client,
                    tunnel: args.tunnel,
                    user: args.user,
                    key: args.key,
                    key_passphrase: args.key_passphrase,
                    term: args.term,
                },
                DefaultTokioExecutor::default(),
            )
            .await?;
        }
    }

    Ok(())
}
