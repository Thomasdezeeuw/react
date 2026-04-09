#![feature(async_iterator)]

use std::async_iter::AsyncIterator;
use std::env::args_os;
use std::ffi::{OsStr, OsString};
use std::future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, ExitCode, Stdio};
use std::task::{self, Poll};
use std::time::Duration;

use heph::actor_ref::{ActorRef, SendError};
use heph::supervisor::SupervisorStrategy;
use heph::{actor, actor_fn, from_message};
use heph_rt::access::SubmissionQueue;
use heph_rt::fs::notify::{self, Interest, Recursive};
use heph_rt::io::{Read, stdin};
use heph_rt::process::{self, To};
use heph_rt::spawn::options::ActorOptions;
use heph_rt::timer::{DeadlinePassed, Timer};
use heph_rt::{Access, ThreadLocal, fs};

use std::io;

/// Grace period in which we coalesce file system events.
const GRACE_PERIOD: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    // Set stdin to noncanonical mode so that we can read character immediately.
    let mut set_c_lflag = None;
    let mut termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } == 0 {
        set_c_lflag = Some(termios.c_lflag);
        termios.c_lflag &= !(libc::ICANON | libc::ECHO);
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) };
    }

    let result = try_main();

    // Reset the stdin mode.
    if let Some(c_lflag) = set_c_lflag {
        termios.c_lflag = c_lflag;
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) };
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if !err.is_empty() {
                eprintln!("{err}");
            }
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<(), String> {
    std_logger::Config::logfmt().init();

    let mut clear_screen = false;
    let mut restart = false;
    let mut paths = Vec::new();
    let mut args = args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-c" || arg == "--clear" {
            clear_screen = true;
        } else if arg == "-r" || arg == "--restart" {
            restart = true;
        } else if arg == "--" {
            break;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if paths.is_empty() {
        return Err("No paths to watch".into());
    }
    let Some(cmd) = args.next() else {
        return Err("No command to run".into());
    };
    let cmd_args = args.collect();

    let mut rt = heph_rt::Runtime::setup()
        .num_threads(1)
        .build()
        .map_err(|err| err.to_string())?;

    // TODO: set exit from actor failing.

    rt.run_on_workers(move |mut rt| -> Result<(), heph_rt::Error> {
        let process_ref = rt.spawn_local(
            supervisor,
            actor_fn(process_actor),
            (cmd, cmd_args, clear_screen, restart),
            ActorOptions::default(),
        );
        rt.receive_signals(process_ref.clone().map());

        let watcher_ref = rt.spawn_local(
            supervisor,
            actor_fn(fs_watcher),
            (process_ref.clone(), paths),
            ActorOptions::default(),
        );
        rt.receive_signals(watcher_ref.clone());

        let tty_ref = rt.spawn_local(
            supervisor,
            actor_fn(tty_actor),
            (process_ref, watcher_ref),
            ActorOptions::default(),
        );
        rt.receive_signals(tty_ref);

        Ok(())
    })
    .map_err(|err| err.to_string())?;
    rt.start().map_err(|err| err.to_string())
}

fn supervisor<T>(err: String) -> SupervisorStrategy<T> {
    eprintln!("{err}");
    SupervisorStrategy::Stop
}

async fn fs_watcher(
    mut ctx: actor::Context<process::Signal, ThreadLocal>,
    process_ref: ActorRef<ProcessMessage>,
    paths: Vec<PathBuf>,
) -> Result<(), String> {
    let interest = Interest::MODIFY
        | Interest::METADATA
        | Interest::MOVE
        | Interest::CREATE
        | Interest::DELETE
        | Interest::DELETE_SELF
        | Interest::MOVE_SELF;

    let mut watcher = fs::Watcher::new(ctx.runtime().sq())
        .map_err(|err| format!("failed to setup file system watcher: {err}"))?;
    for path in paths {
        path.canonicalize()
            .and_then(|path| watcher.watch(path, interest, Recursive::All))
            .map_err(|err| format!("failed to watch path '{}': {err}", path.display()))?;
    }

    let rt = ctx.runtime().clone();
    let mut fut = MultiFuture {
        events: watcher.events(),
        timer: None,
        signal: ctx.receive_messages(),
    };

    loop {
        let event = match Pin::new(&mut fut).await {
            Some(Ok(Ok(event))) => event,
            Some(Ok(Err(err))) => return Err(format!("failed to watch file system: {err}")),
            Some(Err(Ok(signal))) => {
                if signal.should_exit() {
                    return Ok(());
                }
                continue;
            }
            Some(Err(Err(DeadlinePassed))) => {
                if let Err(SendError) = process_ref.send(ProcessMessage::Start).await {
                    return Ok(());
                }
                continue;
            }
            None => return Ok(()),
        };

        let MultiFuture { events, timer, .. } = &mut fut;

        if timer.is_none() {
            *timer = Some(Timer::after(rt.clone(), GRACE_PERIOD));
        }

        let path = events.path_for(&event);
        log::debug!("fs notification for path '{}': {event:?})", path.display());

        if event.file_created() || event.file_moved_into() {
            // Watch newly created directories.
            if let Ok(path) = path.canonicalize() {
                // Only watch directories as file are already watched due to use
                // watching the parent directory.
                if let Err(err) = events.watch(path.clone(), interest, Recursive::All) {
                    if err.kind() != io::ErrorKind::NotADirectory {
                        log::error!(
                            "failed to watch path '{}': {err}, not watching it",
                            path.display()
                        );
                    }
                }
            }
        }
    }
}

struct MultiFuture<'a> {
    events: notify::Events<'a>,
    timer: Option<Timer<ThreadLocal>>,
    signal: actor::ReceiveMessages<'a, process::Signal>,
}

impl<'a> Future for MultiFuture<'a> {
    type Output =
        Option<Result<io::Result<&'a notify::Event>, Result<process::Signal, DeadlinePassed>>>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.events).poll_next(ctx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(Ok(Ok(event)))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Ok(Err(err)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if let Some(timer) = self.timer.as_mut() {
                    if let Poll::Ready(DeadlinePassed) = Pin::new(timer).poll(ctx) {
                        self.timer = None;
                        return Poll::Ready(Some(Err(Err(DeadlinePassed))));
                    }
                }

                match Pin::new(&mut self.signal).poll_next(ctx) {
                    Poll::Ready(Some(signal)) => Poll::Ready(Some(Err(Ok(signal)))),
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

/// Actor that manages the commands via stdin.
async fn tty_actor(
    mut ctx: actor::Context<process::Signal, ThreadLocal>,
    process_ref: ActorRef<ProcessMessage>,
    watcher_ref: ActorRef<process::Signal>,
) -> Result<(), String> {
    let stdin = stdin(ctx.runtime().sq());
    let mut fut = EitherFuture {
        read: stdin.read(Vec::with_capacity(1)),
        signal: ctx.receive_messages(),
    };
    loop {
        let mut buf = match Pin::new(&mut fut).await {
            Ok(Ok(buf)) => buf,
            Ok(Err(err)) => return Err(format!("failed to read from stdin: {err}")),
            Err(Ok(signal)) => {
                if signal.should_exit() {
                    return Ok(());
                }
                continue;
            }
            Err(Err(actor::NoMessages)) => return Ok(()),
        };

        if buf.is_empty() {
            return Ok(());
        }
        match buf[0] {
            b' ' => {
                if let Err(_) = process_ref.send(ProcessMessage::Start).await {
                    return Ok(());
                }
            }
            b'q' => {
                _ = process_ref
                    .send(ProcessMessage::Signal(process::Signal::INTERRUPT))
                    .await;
                _ = watcher_ref.send(process::Signal::INTERRUPT).await;
                return Ok(());
            }
            _ => {}
        }

        buf.clear();
        fut.read = stdin.read(buf);
    }
}

struct EitherFuture<'a> {
    read: Read<'a, Vec<u8>>,
    signal: actor::ReceiveMessages<'a, process::Signal>,
}

impl<'a> Future for EitherFuture<'a> {
    type Output = Result<io::Result<Vec<u8>>, Result<process::Signal, actor::NoMessages>>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.read).poll(ctx) {
            Poll::Ready(res) => Poll::Ready(Ok(res)),
            Poll::Pending => match Pin::new(&mut self.signal).poll_next(ctx) {
                Poll::Ready(Some(signal)) => Poll::Ready(Err(Ok(signal))),
                Poll::Ready(None) => Poll::Ready(Err(Err(actor::NoMessages))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Actor that manages the started process.
#[allow(unused_assignments)] // Want to be explicit in setting process to None.
async fn process_actor(
    mut ctx: actor::Context<ProcessMessage, ThreadLocal>,
    cmd: OsString,
    cmd_args: Vec<OsString>,
    clear_screen: bool,
    restart: bool,
) -> Result<(), String> {
    let sq = ctx.runtime().sq();
    let mut process = Some(start_process(sq.clone(), &cmd, &cmd_args, clear_screen)?);
    while let Ok(msg) = ctx.receive_next().await {
        match msg {
            ProcessMessage::Start => {
                if restart {
                    send_signal(&mut process, process::Signal::INTERRUPT).await?;
                } else if let Some(p) = process.as_mut() {
                    let done = future::poll_fn(|ctx| match Pin::new(&mut *p).poll(ctx) {
                        Poll::Ready(Ok(_)) => Poll::Ready(Ok(true)),
                        Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                        Poll::Pending => Poll::Ready(Ok(false)),
                    })
                    .await
                    .map_err(|err| format!("failed to wait on process: {err}"))?;
                    if !done {
                        continue;
                    }
                }
                process = Some(start_process(sq.clone(), &cmd, &cmd_args, clear_screen)?);
            }
            ProcessMessage::Signal(signal) => {
                send_signal(&mut process, signal).await?;
                if signal.should_exit() {
                    return Ok(());
                }
            }
        }
    }

    send_signal(&mut process, process::Signal::INTERRUPT).await?;

    Ok(())
}

fn start_process(
    sq: SubmissionQueue,
    cmd: &OsStr,
    cmd_args: &[OsString],
    clear_screen: bool,
) -> Result<process::WaitId, String> {
    if clear_screen {
        crate::clear_screen();
    }
    let child = Command::new(cmd)
        .args(cmd_args.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("failed to start process: {err}"))?;
    Ok(process::wait_on(sq, &child).flags(process::WaitOption::EXITED))
}

async fn send_signal(
    process: &mut Option<process::WaitId>,
    signal: process::Signal,
) -> Result<(), String> {
    let Some(p) = process.as_mut() else {
        return Ok(());
    };

    let res = future::poll_fn(|ctx| match Pin::new(&mut *p).poll(ctx) {
        Poll::Ready(res) => Poll::Ready(Some(res)),
        Poll::Pending => Poll::Ready(None),
    })
    .await;
    match res {
        Some(Ok(_)) => *process = None, // Process is done.
        Some(Err(err)) => {
            *process = None;
            return Err(format!("failed to wait on process: {err}"));
        }
        // Process is not done yet.
        None => {
            let pid = match p.waiting_on() {
                process::WaitOn::Process(id) => id,
                _ => unreachable!(),
            };
            process::send_signal(To::Process(pid), signal)
                .map_err(|err| format!("failed to relay process signal: {err}"))?;

            if signal.should_exit() {
                p.await
                    .map_err(|err| format!("failed to wait on process: {err}"))?;
                *process = None;
            }
        }
    }
    Ok(())
}

enum ProcessMessage {
    /// Start, or restart, the process.
    Start,
    /// Relay a signal to the process.
    Signal(process::Signal),
}

from_message!(ProcessMessage::Signal(process::Signal));

fn clear_screen() {
    // 2J clear screen, 3J clear scrollback buffer, H reset cursor to top left.
    // See <https://en.wikipedia.org/wiki/ANSI_escape_code#Control_Sequence_Introducer_commands>
    let buf = b"\x1b[2J\x1b[3J\x1b[H";
    let _ = std::io::Write::write(&mut io::stdout(), buf);
    let _ = std::io::Write::flush(&mut io::stdout());
}
