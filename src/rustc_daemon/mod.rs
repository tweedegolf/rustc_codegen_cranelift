use std::env;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::str::from_utf8;

use rustc_driver::{
    DEFAULT_BUG_REPORT_URL, TimePassesCallbacks, args, catch_with_exit_code, init_rustc_env_logger,
    install_ctrlc_handler, install_ice_hook, run_compiler,
};
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::ErrorOutputType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompileTask {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

pub fn main() -> ExitCode {
    if env::var("__CG_CLIF_DAEMON_START").is_ok_and(|v| v == "1") {
        run_daemon()
    } else {
        invoke_daemon()
    }
}

fn invoke_daemon() -> ExitCode {
    // Build up the compilation task
    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
    let args = args::raw_args(&early_dcx);
    let env: Vec<_> = env::vars().collect();

    let task = CompileTask { args, env };

    let socket_path = socket_path();
    let mut retrying = false;
    let mut session = loop {
        match UnixStream::connect(&socket_path) {
            Ok(session) => break session,
            Err(error) => {
                if retrying {
                    println!("Cannot connect to daemon: {error}");
                    return ExitCode::FAILURE;
                } else {
                    retrying = true;
                    start_daemon();
                }
            }
        }
    };

    serde_json::to_writer(&mut session, &task).expect("Unable to send task to daemon");
    session.shutdown(std::net::Shutdown::Write).expect("Could not shutdown write connection");

    let _ = std::io::copy(&mut session, &mut std::io::stderr());

    ExitCode::SUCCESS
}

fn start_daemon() {
    let directory =
        tempfile::tempdir().expect("Unable to get temporary directory for notification socket");
    let mut notify_path = directory.path().to_owned();
    notify_path.push("sock");
    let listener =
        UnixListener::bind(&notify_path).expect("Unable to open startup notification socket");
    let _child = Command::new(env::current_exe().expect("Unable to get current executable"))
        .env("__CG_CLIF_DAEMON_START", "1")
        .env("__CG_CLIF_DAEMON_NOTIFY", notify_path)
        .spawn()
        .expect("Unable to start daemon");

    let (mut socket, _) = listener.accept().expect("Unable to head start of daemon");
    let mut status = vec![];
    socket.read_to_end(&mut status).expect("Unable to read startup status");
    println!("{}", std::str::from_utf8(&status).expect("Unable to read startup status"));
}

fn run_daemon() -> ExitCode {
    daemonix::Daemonize::new().start().expect("Unable to become a daemon");

    let socket_path = socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(socket_path);

    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());

    init_rustc_env_logger(&early_dcx);
    let mut callbacks = TimePassesCallbacks::default();
    install_ice_hook(DEFAULT_BUG_REPORT_URL, |_| ());

    if let Ok(path) = env::var("__CG_CLIF_DAEMON_NOTIFY") {
        let mut notify = UnixStream::connect(path).expect("Unable to notify startup");
        notify.write_all(b"Started").expect("Unable to notify startup");
    }

    let listener = listener.expect("Unable to open socket");

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(request) = serde_json::from_reader::<_, CompileTask>(&mut stream) {
                    stream
                        .write_all(format!("Received request: {:?}\n", request).as_bytes())
                        .expect("Failed to send feedback");
                    stream.flush().expect("Could not flush");
                    catch_with_exit_code(|| run_compiler(&request.args, &mut callbacks));
                }
                stream.write_all(b"\ndone\n").expect("Failed to send feedback");
                stream.flush().expect("Could not flush");
            }
            Err(_) => todo!(),
        }
    }
}

fn socket_path() -> PathBuf {
    let mut socket_path: PathBuf =
        env::var_os("XDG_RUNTIME_DIR").expect("Missing runtime directory").into();
    socket_path.push("cg_clif_daemon");

    socket_path
}
