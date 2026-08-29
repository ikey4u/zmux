use std::io::{self, Write};

use clap::{Parser, Subcommand};
use zmux::{
    client::ClientApp,
    platform::{setup_signals, ZMUX_VERSION},
};

#[derive(Parser)]
#[command(
    name = "zmux",
    version = ZMUX_VERSION,
    about = "Cross-platform terminal multiplexer",
    after_help = "Examples:\n  Start or attach to the default zmux server:\n    zmux\n\n  Start an isolated test instance without touching your current session:\n    zmux --clean -L test-scroll\n\n  Attach to all running servers as tabs:\n    zmux a\n\n  Attach only to the selected socket:\n    zmux -L test-scroll a --single\n\n  List sessions for that isolated test instance:\n    zmux -L test-scroll ls\n\n  Start in a specific working directory:\n    zmux -c /path/to/project\n\n  Open the runtime options panel inside zmux:\n    Prefix + O"
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
        #[arg(short = 't', long = "tab")]
        tab: Option<String>,
    },
    #[command(name = "a", alias = "attach", alias = "attach-session")]
    Attach {
        #[arg(short = 't', long)]
        target: Option<String>,
        #[arg(short = 'a', long, hide = true)]
        all: bool,
        #[arg(long)]
        single: bool,
    },
    #[command(name = "ls", alias = "list-sessions")]
    Ls,
    #[command(name = "kill-server")]
    KillServer {
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(value_name = "SOCKET")]
        sockets: Vec<String>,
    },
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
        Some(Cmd::New { session, tab }) => {
            ClientApp::new_with_initial_tab_title(
                &socket,
                session,
                cli.clean,
                cli.directory.clone(),
                tab,
            )
            .run()?;
        }
        Some(Cmd::Attach { target, single, .. }) => {
            if single {
                ClientApp::new(
                    &socket,
                    target,
                    cli.clean,
                    cli.directory.clone(),
                )
                .run()?;
            } else {
                ClientApp::new_attach_all(
                    &socket,
                    target,
                    cli.clean,
                    cli.directory.clone(),
                )
                .run()?;
            }
        }
        Some(Cmd::Ls) => {
            run_ls(&socket)?;
        }
        Some(Cmd::KillServer { all, sockets }) => {
            run_kill_server(&socket, sockets, all)?;
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

    zmux::server::install_server_panic_hook();

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
    let mut outputs = Vec::new();
    for name in matching_socket_names(socket_name)? {
        if let Some(output) = list_server_sessions(&name)? {
            outputs.push((name, output));
        }
    }

    if outputs.is_empty() {
        println!("no server running on socket '{}'", socket_name);
        return Ok(());
    }

    let multi = outputs.len() > 1;
    for (index, (name, output)) in outputs.into_iter().enumerate() {
        if index > 0 {
            println!();
        }
        if multi {
            println!("{}:", name);
        }
        println!("{}", output);
    }
    Ok(())
}

fn list_server_sessions(socket_name: &str) -> io::Result<Option<String>> {
    use std::io::BufReader;

    use zmux::ipc::{connect_client, recv_resp};

    let stream = match connect_client(socket_name) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let mut write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write_stream.write_all(b"LIST\n")?;
    write_stream.flush()?;
    Ok(Some(recv_resp(&mut reader)?))
}

fn run_kill_server(
    socket_name: &str,
    sockets: Vec<String>,
    all: bool,
) -> io::Result<()> {
    use std::collections::BTreeSet;

    let targets = if all {
        all_socket_names(socket_name)?
    } else if sockets.is_empty() {
        vec![socket_name.to_string()]
    } else {
        sockets
    };
    let targets = targets.into_iter().collect::<BTreeSet<_>>();

    if targets.is_empty() {
        println!("no zmux servers running");
        return Ok(());
    }

    let mut killed = 0usize;
    for name in targets {
        if kill_server(&name)? {
            killed += 1;
            println!("killed server '{}'", name);
        } else {
            println!("no server running on socket '{}'", name);
        }
    }

    if killed == 0 {
        println!("no zmux servers killed");
    }
    Ok(())
}

fn kill_server(socket_name: &str) -> io::Result<bool> {
    use std::io::BufReader;

    use zmux::ipc::{connect_client, recv_resp};

    let stream = match connect_client(socket_name) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let mut write_stream = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write_stream.write_all(b"KILL_SERVER\n")?;
    write_stream.flush()?;
    let _ = recv_resp(&mut reader)?;
    Ok(true)
}

#[cfg(unix)]
fn all_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::{collections::BTreeSet, os::unix::fs::FileTypeExt};

    let socket_path = zmux::ipc::socket_path(socket_name)?;
    let Some(dir) = socket_path.parent() else {
        return Ok(Vec::new());
    };
    let mut names = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_socket() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(windows)]
fn all_socket_names(_socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let pipe_prefix = "zmux-";
    let mut names = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
        for entry in entries.flatten() {
            let pipe_name = entry.file_name().to_string_lossy().to_string();
            if let Some(socket) = pipe_name.strip_prefix(pipe_prefix) {
                names.insert(socket.to_string());
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(unix)]
fn matching_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let socket_path = zmux::ipc::socket_path(socket_name)?;
    let Some(dir) = socket_path.parent() else {
        return Ok(vec![socket_name.to_string()]);
    };
    let tab_prefix = format!("{}.tab.", socket_name);
    let mut names = BTreeSet::new();
    if socket_path.exists() {
        names.insert(socket_name.to_string());
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string)
            else {
                continue;
            };
            if name.starts_with(&tab_prefix) {
                names.insert(name);
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(windows)]
fn matching_socket_names(socket_name: &str) -> io::Result<Vec<String>> {
    use std::collections::BTreeSet;

    let pipe_prefix = "zmux-";
    let base_pipe = format!("{}{}", pipe_prefix, socket_name);
    let tab_pipe_prefix = format!("{}{}.tab.", pipe_prefix, socket_name);
    let mut names = BTreeSet::new();

    if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
        for entry in entries.flatten() {
            let pipe_name = entry.file_name().to_string_lossy().to_string();
            if pipe_name == base_pipe {
                names.insert(socket_name.to_string());
            } else if pipe_name.starts_with(&tab_pipe_prefix) {
                if let Some(socket) = pipe_name.strip_prefix(pipe_prefix) {
                    names.insert(socket.to_string());
                }
            }
        }
    }

    if names.is_empty() {
        names.insert(socket_name.to_string());
    }
    Ok(names.into_iter().collect())
}
