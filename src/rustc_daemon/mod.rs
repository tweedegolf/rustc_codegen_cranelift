use std::cell::OnceCell;
use std::fs::OpenOptions;
use std::hash::Hash;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::{env, io};

use anstream::AutoStream;
use postcard::Deserializer;
use rustc_data_structures::jobserver;
use rustc_data_structures::sync::RwLock;
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

use crate::CraneliftCodegenBackend;
use crate::unwind_module::InMemoryCache;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
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

    let env: Vec<_> = env::vars().collect();
    let working_directory = env::current_dir().expect("Unable to get current directory");

    let task = CompileTask { args: args.clone(), env, working_directory };

    let at_args = args.get(1..).unwrap_or_default();

    let early_args = args::arg_expand_all(&early_dcx, at_args);

    match handle_options(&early_dcx, &early_args) {
        HandledOptions::None => ExitCode::SUCCESS,
        HandledOptions::Normal(matches)
            if !matches.opt_present("print") && !matches.free.contains(&"-".to_string()) =>
        {
            let socket_path = socket_path();
            let mut retrying = false;
            let mut session = loop {
                match UnixStream::connect(&socket_path) {
                    Ok(session) => break session,
                    Err(_) => {
                        if retrying {
                            return ExitCode::FAILURE;
                        } else {
                            retrying = true;
                            start_daemon();
                        }
                    }
                }
            };

            serde_json::to_writer(&mut session, &task)
                .map_err(|e| {
                    eprintln!("task: {:?}", task);
                    e
                })
                .expect("Unable to send task to daemon");
            session
                .shutdown(std::net::Shutdown::Write)
                .expect("Could not shutdown write connection");

            let mut buffer = [0u8; 4096];
            let reader = postcard::de_flavors::io::io::IOReader::new(&mut session, &mut buffer);
            let mut deserializer = Deserializer::from_flavor(reader);

            while let Ok(message) = JsonStreamMessage::deserialize(&mut deserializer) {
                match message {
                    JsonStreamMessage::ExitSucces => return ExitCode::SUCCESS,
                    JsonStreamMessage::ExitFailure => return ExitCode::FAILURE,
                    JsonStreamMessage::OutputBytes(items) => {
                        std::io::stderr().write_all(&items).ok();
                    }
                }
            }

            ExitCode::FAILURE
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
    output_stream: Arc<RwLock<UnixStream>>,
    env: Vec<(String, String)>,
    working_directory: PathBuf,
    cache: InMemoryCache,
}

// Implementation originally from sccache.
pub unsafe fn discard_inherited_jobserver() {
    if let Some(value) = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"]
        .into_iter()
        .find_map(|env| std::env::var(env).ok())
        && let Some(auth) = value.rsplit(' ').find_map(|arg| {
            arg.strip_prefix("--jobserver-auth=").or_else(|| arg.strip_prefix("--jobserver-fds="))
        })
        && !auth.starts_with("fifo:")
    {
        let mut parts = auth.splitn(2, ',');
        let read = parts.next().unwrap();
        let write = match parts.next() {
            Some(w) => w,
            None => return,
        };
        let read = read.parse().unwrap();
        let write = write.parse().unwrap();
        if read < 0 || write < 0 {
            return;
        }
        unsafe {
            if libc::fcntl(read, libc::F_GETFD) == -1 {
                return;
            }
            if libc::fcntl(write, libc::F_GETFD) == -1 {
                return;
            }
            libc::close(read);
            libc::close(write);
        }
    }
}

fn run_daemon() -> ExitCode {
    // We need to discard the parent's job server otherwise we could keep it around for unrelated compilation sessions.
    // Safe because nothing is going on yet.
    unsafe {
        discard_inherited_jobserver();
    }

    for key in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
        if let Ok(value) = std::env::var(key) {
            let new_value = value
                .rsplit(' ')
                .filter(|flag| !flag.starts_with("--jobserver"))
                .collect::<Vec<_>>()
                .join(" ");
            // Safe because we are running single-threaded at this point.
            unsafe {
                env::set_var(key, new_value);
            }
        }
    }

    let log_file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path())
        .expect("Unable to open log file");

    daemonix::Daemonize::new()
        .stdout(log_file.try_clone().expect("unable to reopen log file"))
        .stderr(log_file)
        .start()
        .expect("Unable to become a daemon");

    let socket_path = socket_path();
    let lock_path = lock_path();

    let listener = {
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)
            .expect("Could not open lockfile.");
        lock_file.lock().expect("Unable to lock lockfile");
        if UnixStream::connect(&socket_path).is_ok() {
            Err(std::io::ErrorKind::AddrInUse.into())
        } else {
            let _ = std::fs::remove_file(&socket_path);
            UnixListener::bind(socket_path)
        }
    };

    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());

    init_rustc_env_logger(&early_dcx);
    rustc_data_structures::jobserver::initialize(
        std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
        |err| {
            let note = "the build environment is likely misconfigured";
            early_dcx.early_struct_warn(err).with_note(note).emit()
        },
    );
    install_ice_hook(DEFAULT_BUG_REPORT_URL, |_| ());

    if let Ok(path) = env::var("__CG_CLIF_DAEMON_NOTIFY") {
        let mut notify = UnixStream::connect(path).expect("Unable to notify startup");
        notify.write_all(b"Started").expect("Unable to notify startup");
    }

    let Ok(listener) = listener else {
        return ExitCode::SUCCESS;
    };

    let cache = InMemoryCache::default();

    loop {
        let token = jobserver::client().acquire().expect("Unable to acquire jobserver token");
        match listener.accept() {
            Ok((mut stream, _)) => {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    if let Ok(request) = serde_json::from_reader::<_, CompileTask>(&mut stream) {
                        let stream = Arc::new(RwLock::new(stream));
                        if catch_with_exit_code(|| {
                            run_compiler(
                                &request.args,
                                &mut DaemonCallbacks {
                                    output_stream: stream.clone(),
                                    env: request.env.clone(),
                                    working_directory: request.working_directory.clone(),
                                    cache,
                                },
                            )
                        }) != ExitCode::SUCCESS
                        {
                            eprintln!("task failed: {:?}", request);
                            let mut writer = stream.write();
                            postcard::to_io(&JsonStreamMessage::ExitFailure, &mut *writer).ok();
                        } else {
                            let mut writer = stream.write();
                            postcard::to_io(&JsonStreamMessage::ExitSucces, &mut *writer).ok();
                        }
                    } else {
                        postcard::to_io(&JsonStreamMessage::ExitFailure, &mut stream).ok();
                    }
                    // Explicit token to keep track that we are running a job.
                    drop(token);
                });
            }
            Err(_) => todo!(),
        }
    }
}

impl Callbacks for DaemonCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        let options = config.opts.clone();
        let output_stream = self.output_stream.clone();

        let cache = self.cache.clone();

        config.make_codegen_backend = Some(Box::new(|_sess| {
            Box::new(CraneliftCodegenBackend { config: OnceCell::new(), cache: Some(cache) })
        }));

        match &mut config.input {
            config::Input::File(path_buf) => {
                let mut new_path = self.working_directory.clone();
                new_path.push(&*path_buf);
                *path_buf = new_path;
            }
            config::Input::Str { .. } => {}
        }

        for (key, value) in self.env.drain(..) {
            config.opts.logical_env.insert(key, value);
        }

        config.psess_created = Some(Box::new(move |parse_sess| {
            parse_sess.dcx().set_emitter(server_emitter(
                output_stream,
                &options,
                parse_sess.clone_source_map(),
            ));
        }));
    }
}

#[derive(Deserialize, Serialize)]
enum JsonStreamMessage {
    ExitSucces,
    ExitFailure,
    OutputBytes(Vec<u8>),
}

struct JsonStreamSender {
    inner: Arc<RwLock<UnixStream>>,
}

impl JsonStreamSender {
    fn new(inner: Arc<RwLock<UnixStream>>) -> Box<Self> {
        Box::new(JsonStreamSender { inner })
    }
}

impl Write for JsonStreamSender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut writer = self.inner.write();

        postcard::to_io(&JsonStreamMessage::OutputBytes(buf.into()), &mut *writer)
            .map_err(std::io::Error::other)?;

        writer.flush()?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        /* noop as we flush on each write. */
        Ok(())
    }
}

fn server_emitter(
    stream: Arc<RwLock<UnixStream>>,
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
        config::ErrorOutputType::HumanReadable { kind, color_config: _ } => {
            match kind {
                HumanReadableErrorType { short, unicode } => {
                    let emitter = AnnotateSnippetEmitter::new(AutoStream::always(
                        JsonStreamSender::new(stream),
                    ))
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
            }
        }
        config::ErrorOutputType::Json { pretty, json_rendered, color_config } => Box::new(
            JsonEmitter::new(
                Box::new(io::BufWriter::new(JsonStreamSender::new(stream))),
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

fn lock_path() -> PathBuf {
    let mut lock_path: PathBuf =
        env::var_os("XDG_RUNTIME_DIR").expect("Missing runtime directory").into();
    lock_path.push("cg_clif_daemon_lock");

    lock_path
}

fn log_path() -> PathBuf {
    let mut log_path: PathBuf =
        env::var_os("XDG_RUNTIME_DIR").expect("Missing runtime directory").into();
    log_path.push("cg_clif_daemon_log");

    log_path
}
