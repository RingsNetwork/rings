use clap::{Args, Parser, Subcommand};
use daemonize::Daemonize;
use futures::lock::Mutex;
use libc::kill;
use rings_core::message::MessageHandler;
use rings_core::session::SessionManager;
use rings_core::swarm::Swarm;
use rings_core::types::message::MessageListener;
use rings_core::{dht::PeerRing, ecc::SecretKey};
use rings_node::logger::{LogLevel, Logger};
use rings_node::service::{run_service, run_udp_turn};
use std::fs::{self, File};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[clap(about)]
struct Cli {
    #[clap(long, short = 'v', default_value_t = LogLevel::Info, arg_enum)]
    log_level: LogLevel,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Run(Box<RunArgs>),
    Shutdown(ShutdownArgs),
}

#[derive(Args, Debug)]
struct RunArgs {
    #[clap(long, short = 'b', default_value = "127.0.0.1:50000", env)]
    pub http_addr: String,

    #[clap(long, short = 's', default_value = "stun://stun.l.google.com:19302")]
    pub ice_server: String,

    #[clap(
        long = "eth",
        short = 'e',
        default_value = "http://127.0.0.1:8545",
        env
    )]
    pub eth_endpoint: String,

    #[clap(long = "key", short = 'k', env)]
    pub eth_key: SecretKey,

    #[clap(long, short = 'p', default_value = "/tmp/rings-node.pid")]
    pub pid_file: String,

    #[clap(long, default_value = "nobody")]
    pub user: String,

    #[clap(long, default_value = "daemon")]
    pub group: String,

    #[clap(long, short = 'w', default_value = "/")]
    pub work_dir: String,

    /// STUN server address.
    #[clap(long, default_value = "3478")]
    pub turn_port: String,

    /// STUN publicip.
    #[clap(long, default_value = "127.0.0.1")]
    pub public_ip: String,

    /// Username.
    #[clap(long, default_value = "rings")]
    pub username: String,

    /// Password.
    #[clap(long, default_value = "password")]
    pub password: String,

    /// Realm.
    /// REALM
    /// The REALM attribute is present in Shared Secret Requests and Shared
    /// Secret Responses. It contains text which meets the grammar for
    /// "realm" as described in RFC 3261, and will thus contain a quoted
    /// string (including the quotes).
    #[clap(long, default_value = "rings")]
    pub realm: String,

    #[clap(long)]
    pub disable_turn: bool,
}

#[derive(Args, Debug)]
struct ShutdownArgs {
    #[clap(long, short = 'p', default_value = "/tmp/rings-node.pid")]
    pub pid_file: String,
}

async fn run_jobs(args: &RunArgs) -> anyhow::Result<()> {
    let key: &SecretKey = &args.eth_key;
    let dht = Arc::new(Mutex::new(PeerRing::new(key.address().into())));
    let (auth, key) = SessionManager::gen_unsign_info(key.address(), None, None)?;
    let sig = key.sign(&auth.to_string()?).to_vec();
    let session = SessionManager::new(&sig, &auth, &key);

    let swarm = Arc::new(Swarm::new(&args.ice_server, key.address(), session));

    let listen_event = MessageHandler::new(dht.clone(), swarm.clone());
    let swarm_clone = swarm.clone();
    let http_addr = args.http_addr.to_owned();
    if args.disable_turn {
        let (_, _) = futures::join!(async { Arc::new(listen_event).listen().await }, async {
            run_service(http_addr.to_owned(), swarm_clone).await
        },);
    } else {
        let public_ip: &str = &args.public_ip;
        let turn_port: &str = &args.public_ip;
        let username: &str = &args.username;
        let password: &str = &args.password;
        let realm: &str = &args.realm;
        let (_, _, _) = futures::join!(
            async { Arc::new(listen_event).listen().await },
            async { run_service(http_addr.to_owned(), swarm_clone).await },
            async { run_udp_turn(public_ip, turn_port, username, password, realm).await }
        );
    }
    Ok(())
}

fn run_daemon(args: &RunArgs) {
    let stdout = File::create("/tmp/rings-node/info.log").unwrap();
    let stderr = File::create("/tmp/rings-node/err.log").unwrap();

    let daemonize = Daemonize::new()
        .pid_file(args.pid_file.as_str())
        .chown_pid_file(true)
        .working_directory(args.work_dir.as_str())
        .user(args.user.as_str())
        .group(args.group.as_str())
        .stdout(stdout)
        .stderr(stderr);
    if let Err(e) = daemonize.start() {
        panic!("{}", e);
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        if let Err(e) = run_jobs(args).await {
            panic!("{}", e);
        }
    });
}

fn shutdown_daemon(args: &ShutdownArgs) -> anyhow::Result<()> {
    let pid: i32 = fs::read_to_string(args.pid_file.as_str())?.parse()?;
    unsafe {
        kill(pid, 9);
    }
    println!("Killed: {}", pid);
    Ok(())
}

fn main() {
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    Logger::init(cli.log_level.into()).expect("log err");

    match cli.command {
        Command::Run(args) => {
            run_daemon(&args);
        }
        Command::Shutdown(args) => {
            if let Err(e) = shutdown_daemon(&args) {
                panic!("{}", e);
            }
        }
    };
}
