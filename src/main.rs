use std::io::{self, Write};

use clap::{Parser, Subcommand};
use zmux::{client::ClientApp, platform::setup_signals};

#[derive(Parser)]
#[command(
    name = "zmux",
    version,
    about = "Cross-platform terminal multiplexer",
    after_help = "Examples:\n  Start or attach to the default zmux server:\n    zmux\n\n  Start an isolated test instance without touching your current session:\n    zmux --clean -L test-scroll\n\n  Attach to that isolated test instance from another terminal:\n    zmux -L test-scroll a\n\n  List sessions for that isolated test instance:\n    zmux -L test-scroll ls\n\n  Start in a specific working directory:\n    zmux -c /path/to/project\n\n  Open the runtime options panel inside zmux:\n    Prefix + O"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[arg(short = 'L', long, default_value = "default")]
    socket: String,

    #[arg(short = 's', long)]
    session: Option<String>,

    #[arg(long)]
    clean: bool,

    #[arg(short = 'c', long = "directory", value_name = "DIR")]
    directory: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(name = "new", alias = "new-session")]
    New {
        #[arg(short = 's', long)]
        session: Option<String>,
    },
    #[command(name = "a", alias = "attach", alias = "attach-session")]
    Attach {
        #[arg(short = 't', long)]
        target: Option<String>,
    },
    #[command(name = "ls", alias = "list-sessions")]
    Ls,
    #[command(name = "server")]
    Server,
    #[clap(external_subcommand)]
    External(Vec<String>),
}

fn main() -> io::Result<()> {
    setup_signals();

    let cli = Cli::parse();
    let socket = cli.socket.clone();

    match cli.command {
        Some(Cmd::Server) => {
            run_server_daemon(
                &socket,
                cli.session.as_deref(),
                cli.directory.as_deref(),
            )?;
        }
        Some(Cmd::New { session }) => {
            ClientApp::new(&socket, session, cli.clean, cli.directory.clone())
                .run()?;
        }
        Some(Cmd::Attach { target }) => {
            ClientApp::new(&socket, target, cli.clean, cli.directory.clone())
                .run()?;
        }
        Some(Cmd::Ls) => {
            run_ls(&socket)?;
        }
        Some(Cmd::External(args)) => {
            eprintln!("unknown subcommand: {:?}", args);
            std::process::exit(1);
        }
        None => {
            ClientApp::new(&socket, cli.session, cli.clean, cli.directory)
                .run()?;
        }
    }

    Ok(())
}

fn run_server_daemon(
    socket_name: &str,
    session_name: Option<&str>,
    start_dir: Option<&str>,
) -> io::Result<()> {
    use zmux::{server::InProcessServer, types::session::Size};

    #[cfg(unix)]
    zmux::pty::remember_host_termios();

    let session = session_name.unwrap_or("0").to_string();
    let size = Size::new(24, 80);
    let server = InProcessServer::start(
        session,
        size,
        Some(socket_name.to_string()),
        start_dir.map(|dir| dir.to_string()),
    )?;
    server.run_socket_server(socket_name)
}

fn run_ls(socket_name: &str) -> io::Result<()> {
    use std::io::BufReader;

    use zmux::ipc::{connect_client, recv_resp};

    let stream = match connect_client(socket_name) {
        Ok(s) => s,
        Err(_) => {
            println!("no server running on socket '{}'", socket_name);
            return Ok(());
        }
    };
    let mut write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write_stream.write_all(b"LIST\n")?;
    write_stream.flush()?;
    let output = recv_resp(&mut reader)?;
    println!("{}", output);
    Ok(())
}
