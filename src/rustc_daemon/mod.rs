use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::{env, io};

use anstream::AutoStream;
use rustc_driver::{
    Callbacks, DEFAULT_BUG_REPORT_URL, HandledOptions, args, catch_with_exit_code, handle_options,
    init_rustc_env_logger, install_ice_hook, run_compiler,
};
use rustc_errors::TerminalUrl;
use rustc_errors::annotate_snippet_emitter_writer::AnnotateSnippetEmitter;
use rustc_errors::emitter::{DynEmitter, HumanReadableErrorType, OutputTheme};
use rustc_errors::json::JsonEmitter;
use rustc_session::config::ErrorOutputType;
use rustc_session::{EarlyDiagCtxt, config};
use rustc_span::source_map::SourceMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompileTask {
    args: Vec<String>,
    env: Vec<(String, String)>,
    working_directory: PathBuf,
}

pub fn main() -> ExitCode {
    if env::var("__CG_CLIF_DAEMON_START").is_ok_and(|v| v == "1") {
        run_daemon()
    } else {
        invoke_daemon()
    }
}

struct NoopCallbacks;

impl Callbacks for NoopCallbacks {}

fn invoke_daemon() -> ExitCode {
    // Build up the compilation task
    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
    let args = args::raw_args(&early_dcx);

    let at_args = args.get(1..).unwrap_or_default();

    let early_args = args::arg_expand_all(&early_dcx, at_args);

    match handle_options(&early_dcx, &early_args) {
        HandledOptions::None => ExitCode::SUCCESS,
        HandledOptions::Normal(matches) if !matches.opt_present("print") => {
            let env: Vec<_> = env::vars().collect();
            let working_directory = env::current_dir().expect("Unable to get current directory");

            let task = CompileTask { args, env, working_directory };

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
            session
                .shutdown(std::net::Shutdown::Write)
                .expect("Could not shutdown write connection");

            let _ = std::io::copy(&mut session, &mut std::io::stderr());

            ExitCode::SUCCESS
        }
        _ => {
            init_rustc_env_logger(&early_dcx);
            install_ice_hook(DEFAULT_BUG_REPORT_URL, |_| ());

            catch_with_exit_code(|| run_compiler(&args, &mut NoopCallbacks));
            ExitCode::SUCCESS
        }
    }
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
}

struct DaemonCallbacks {
    output_stream: Option<UnixStream>,
    working_directory: PathBuf,
}

fn run_daemon() -> ExitCode {
    daemonix::Daemonize::new().start().expect("Unable to become a daemon");

    let socket_path = socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(socket_path);

    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());

    init_rustc_env_logger(&early_dcx);
    install_ice_hook(DEFAULT_BUG_REPORT_URL, |_| ());

    if let Ok(path) = env::var("__CG_CLIF_DAEMON_NOTIFY") {
        let mut notify = UnixStream::connect(path).expect("Unable to notify startup");
        notify.write_all(b"Started").expect("Unable to notify startup");
    }

    let listener = listener.expect("Unable to open socket");

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                std::thread::spawn(move || {
                    if let Ok(request) = serde_json::from_reader::<_, CompileTask>(&mut stream) {
                        catch_with_exit_code(|| {
                            run_compiler(
                                &request.args,
                                &mut DaemonCallbacks {
                                    output_stream: Some(stream),
                                    working_directory: request.working_directory,
                                },
                            )
                        });
                    }
                });
            }
            Err(_) => todo!(),
        }
    }
}

impl Callbacks for DaemonCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        let options = config.opts.clone();
        let output_stream = self.output_stream.take();

        match &mut config.input {
            config::Input::File(path_buf) => {
                let mut new_path = self.working_directory.clone();
                new_path.push(&*path_buf);
                *path_buf = new_path;
            }
            config::Input::Str { .. } => {}
        }

        config.psess_created = Some(Box::new(move |parse_sess| {
            if let Some(stream) = output_stream {
                parse_sess.dcx().set_emitter(server_emitter(
                    stream,
                    &options,
                    parse_sess.clone_source_map(),
                ));
            }
        }));
    }
}

fn server_emitter(
    stream: UnixStream,
    sopts: &config::Options,
    source_map: Arc<SourceMap>,
) -> Box<DynEmitter> {
    let macro_backtrace = sopts.unstable_opts.macro_backtrace;
    let track_diagnostics = sopts.unstable_opts.track_diagnostics;
    let terminal_url = match sopts.unstable_opts.terminal_urls {
        TerminalUrl::Auto => {
            match (std::env::var("COLORTERM").as_deref(), std::env::var("TERM").as_deref()) {
                (Ok("truecolor"), Ok("xterm-256color"))
                    if sopts.unstable_features.is_nightly_build() =>
                {
                    TerminalUrl::Yes
                }
                _ => TerminalUrl::No,
            }
        }
        t => t,
    };

    let source_map = if sopts.unstable_opts.link_only { None } else { Some(source_map) };

    match sopts.error_format {
        config::ErrorOutputType::HumanReadable { kind, color_config: _ } => match kind {
            HumanReadableErrorType { short, unicode } => {
                let emitter = AnnotateSnippetEmitter::new(AutoStream::always(Box::new(stream)))
                    .sm(source_map)
                    .short_message(short)
                    .diagnostic_width(sopts.diagnostic_width)
                    .macro_backtrace(macro_backtrace)
                    .track_diagnostics(track_diagnostics)
                    .terminal_url(terminal_url)
                    .theme(if unicode { OutputTheme::Unicode } else { OutputTheme::Ascii })
                    .ignored_directories_in_source_blocks(
                        sopts.unstable_opts.ignore_directory_in_diagnostics_source_blocks.clone(),
                    );
                Box::new(emitter.ui_testing(sopts.unstable_opts.ui_testing))
            }
        },
        config::ErrorOutputType::Json { pretty, json_rendered, color_config } => Box::new(
            JsonEmitter::new(
                Box::new(io::BufWriter::new(stream)),
                source_map,
                pretty,
                json_rendered,
                color_config,
            )
            .ui_testing(sopts.unstable_opts.ui_testing)
            .ignored_directories_in_source_blocks(
                sopts.unstable_opts.ignore_directory_in_diagnostics_source_blocks.clone(),
            )
            .diagnostic_width(sopts.diagnostic_width)
            .macro_backtrace(macro_backtrace)
            .track_diagnostics(track_diagnostics)
            .terminal_url(terminal_url),
        ),
    }
}

fn socket_path() -> PathBuf {
    let mut socket_path: PathBuf =
        env::var_os("XDG_RUNTIME_DIR").expect("Missing runtime directory").into();
    socket_path.push("cg_clif_daemon");

    socket_path
}
