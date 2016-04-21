use serde::ser::Serialize;
use serde_json::{ser, to_value};
use serde_json::de::StreamDeserializer;
use serde_json::Value;
use std::env;
use std::io::{Read, Write, BufRead, BufReader};
use std::fs::File;
use std::{process, thread};
use std::process::{Command, Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

use super::error::Error;
use super::response::{ErrorResponse, RunnerOutput};

const ALGOOUT: &'static str = "/tmp/algoout";
const UNKNOWN_EXIT: i32 = -99;

type RunnerResult = Result<Value, Error>;

// Wrapper around the LangRunnerProcess that uses channels to wait on things
// because several aspects of the LangRunnerProcess are blocking
pub struct LangRunner {
    runner: Arc<RwLock<LangRunnerProcess>>,
    tx: Sender<RunnerResult>,
    rx: Receiver<RunnerResult>,
}

// Struct to manage the `bin/pipe` process
struct LangRunnerProcess {
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    stdin: Option<ChildStdin>,
    child: Mutex<Child>,
    exit_status: Mutex<Option<i32>>,
}

// This blocks until output is available on ALGOOUT
fn get_next_algoout_value() -> Result<Value, Error> {
    // Note: Opening a FIFO read-only pipe blocks until a writer opens it.
    println!("Opening /tmp/algoout FIFO...");
    let algoout = try!(File::open(ALGOOUT));


    // Read and deserialize the single next JSON Value on ALGOOUT
    println!("Deserializing algoout stream...");
    let mut algoout_stream: StreamDeserializer<Value, _> = StreamDeserializer::new(algoout.bytes());
    match algoout_stream.next() {
        Some(next) => match next {
            Ok(out) => Ok(out),
            Err(err) => {
                println!("Failed to deserialize next JSON value from stream: {}", err);
                Err(err.into())
            }
        },
        None => Err(Error::Unexpected("No more JSON to read from the stream".to_owned())),
    }
}

impl LangRunner {
    // Start the runner process, initialize channels, and begin monitoring the runner process for exit
    pub fn start() -> Result<LangRunner, Error> {
        let runner = try!(LangRunnerProcess::start());
        let (tx, rx) = channel();
        let lr = LangRunner {
            runner: Arc::new(RwLock::new(runner)),
            rx: rx,
            tx: tx,
        };
        lr.monitor();
        Ok(lr)
    }

    // Monitor runner - notify receiver channel if exit is encountered
    fn monitor(&self) {
        let tx = self.tx.clone();
        let arc_runner = self.runner.clone();
        thread::spawn(move || {
            loop {
                {
                    let runner = arc_runner.read().expect("Failed to acquire read lock on runner");
                    if let Some(code) = runner.check_exited() {
                        println!("LangRunner monitor thread detected exit: {}", code);
                        if let Err(err) = tx.send(Err(Error::UnexpectedExit(code))) {
                            println!("FATAL: Channel receiver disconnected unexpectedly: {}", err);
                            process::exit(code); // Don't want to just panic a single thread and hang
                        }
                        break;
                    };
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    }

    pub fn write<T: Serialize>(&mut self, input: &T) -> Result<(), Error> {
        let mut runner = self.runner.write().expect("Failed to acquire write lock on runner");
        runner.write(input)
    }

    pub fn wait_for_response_or_exit(&mut self) -> RunnerOutput {
        let tx = self.tx.clone();

        let start = Instant::now();
        thread::spawn(move || {
            if let Err(err) = tx.send(get_next_algoout_value()) {
                println!("FATAL: Channel receiver disconnected unexpectedly: {}", err);
                process::exit(UNKNOWN_EXIT); // Don't want to just panic a single thread and hang
            }
        });

        // Block until receiving message from `get_next_algoout_value` or the monitor thread
        let received = self.rx.recv().expect("Channel sender disconnected unexpectedly");
        let duration = start.elapsed();

        let mut runner_output = match received {
            Ok(response) => RunnerOutput::Completed(response),
            Err(err) => {
                println!("Wait encountered an error: {}", err);
                let response = ErrorResponse::from_error(err);
                RunnerOutput::Exited(to_value(&response))
            }
        };

        let (stdout, stderr) = {
            let runner = self.runner.read().expect("Failed to acquire read lock on runner");
            (runner.consume_stdout(), runner.consume_stderr())
        };

        // Augment output with duration and stdout
        runner_output.set_metadata(duration, stdout, stderr);
        runner_output
    }

    pub fn check_exited(&self) -> Option<i32> {
        let runner = self.runner.read().expect("Failed to acquire read lock on runner");
        runner.check_exited()
    }

    pub fn stop(&mut self) -> i32 {
        match self.check_exited() {
            Some(code) => code,
            None => {
                let mut runner = self.runner
                                     .write()
                                     .expect("Failed to acquire write lock on runner");
                runner.stop()
            }
        }
    }
}

impl LangRunnerProcess {
    fn start() -> Result<LangRunnerProcess, Error> {
        let mut path = try!(env::current_dir());
        path.push("bin/pipe");

        let mut child = try!(Command::new(&path)
                                 .stdin(Stdio::piped())
                                 .stdout(Stdio::piped())
                                 .stderr(Stdio::piped())
                                 .spawn());

        println!("Running PID {}: {}", child.id(), path.to_string_lossy());

        let stdin = try!(child.stdin
                              .take()
                              .ok_or(Error::Unexpected(s!("Failed to open runner's STDIN"))));
        let stdout = try!(child.stdout
                               .take()
                               .ok_or(Error::Unexpected(s!("Failed to open runner's STDOUT"))));
        let stderr = try!(child.stderr
                               .take()
                               .ok_or(Error::Unexpected(s!("Failed to open runner's STDERR"))));

        let child_stdout = Arc::new(Mutex::new(Vec::new()));
        let child_stderr = Arc::new(Mutex::new(Vec::new()));

        // Spawn a thread to collect algorithm stdout
        let arc_stdout = child_stdout.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line_result in reader.lines() {
                match line_result {
                    Ok(line) => match arc_stdout.lock() {
                        Ok(mut lines) => lines.push(line),
                        Err(err) => println!("Failed to get lock on stdout lines: {}", err),
                    },
                    Err(err) => println!("Failed to read line: {}", err),
                }
            }
        });

        // Spawn a thread to collect algorithm stderr
        let arc_stderr = child_stderr.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line_result in reader.lines() {
                match line_result {
                    Ok(line) => match arc_stderr.lock() {
                        Ok(mut lines) => lines.push(line),
                        Err(err) => println!("Failed to get lock on stderr lines: {}", err),
                    },
                    Err(err) => println!("Failed to read line: {}", err),
                }
            }
        });

        Ok(LangRunnerProcess {
            child: Mutex::new(child),
            stdin: Some(stdin),
            stdout: child_stdout,
            stderr: child_stderr,
            exit_status: Mutex::new(None),
        })
    }

    pub fn write<T: Serialize>(&mut self, input: &T) -> Result<(), Error> {
        match self.stdin.as_mut() {
            Some(mut stdin) => {
                println!("Sending data to runner stdin");
                try!(ser::to_writer(&mut stdin, &input));
                try!(stdin.write(b"\n"));
                Ok(())
            }
            None => {
                Err(Error::Unexpected("cannot write to closed runner stdin".to_owned()))
            }
        }
    }

    // This returns and clears the buffered stdout
    pub fn consume_stdout(&self) -> String {
        let arc_stdout = self.stdout.clone();
        let mut lines = arc_stdout.lock().expect("Failed to get lock on stdout lines");
        let mut algo_stdout = lines.join("\n");
        if algo_stdout.chars().last() == Some('\n') {
            let _ = algo_stdout.pop();
        }
        lines.clear();
        algo_stdout
    }

    // This returns and clears the buffered stderr
    pub fn consume_stderr(&self) -> String {
        let arc_stderr = self.stderr.clone();
        let mut lines = arc_stderr.lock().expect("Failed to get lock on stderr lines");
        let mut algo_stderr = lines.join("\n");
        if algo_stderr.chars().last() == Some('\n') {
            let _ = algo_stderr.pop();
        }
        lines.clear();
        algo_stderr
    }

    pub fn check_exited(&self) -> Option<i32> {
        // Check if we've already stored the exit code
        // Also holding lock on self.exit_status to ensure wait_timeout is called safely between threads
        let mut exit_status = self.exit_status.lock().expect("Failed to take exit status lock");
        if exit_status.is_some() {
            return Some(exit_status.unwrap());
        }

        // Now let's do a short wait just to see if the process has exited
        let mut child = self.child.lock().expect("Failed to get lock on runner");
        match child.wait_timeout(Duration::from_millis(10)) {
            Err(err) => {
                println!("Error waiting for runner: {}", err);
                *exit_status = Some(UNKNOWN_EXIT);
                Some(UNKNOWN_EXIT)
            }
            Ok(Some(exit)) => {
                println!("Runner exited - {}", exit);
                let code = exit.code().unwrap_or(UNKNOWN_EXIT);
                *exit_status = Some(code);
                Some(code)
            }
            Ok(None) => None, // Still alive
        }
    }

    pub fn stop(&mut self) -> i32 {
        // Check if we've already stored the exit code
        // Also holding lock on self.exit_status to ensure wait_timeout is called safely between threads
        let mut exit_status = self.exit_status.lock().expect("Failed to take exit status lock");
        if exit_status.is_some() {
            return exit_status.unwrap();
        }

        // Mutably `take` child_stdin out of `self` and drop it
        if let Some(_drop_stdin) = self.stdin.take() {
            println!("Sending EOF to runner stdin.");
        } // _drop_stdin goes out of scope here which results in EOF

        // Now that stdin is closed, we can wait on child
        println!("Waiting for runner to exit...");
        let mut child = self.child.lock().expect("Failed to get lock on runner");
        let code = match child.wait_timeout(Duration::from_secs(3)) {
            Err(err) => {
                println!("Error waiting for runner: {}", err);
                UNKNOWN_EXIT
            }
            Ok(Some(exit)) => {
                println!("Runner exited - {}", exit);
                exit.code().unwrap_or(UNKNOWN_EXIT)
            }
            Ok(None) => {
                println!("Runner did not exit. Killing.");
                if let Err(err) = child.kill() {
                    println!("Failed to kill runner: {}", err);
                }
                UNKNOWN_EXIT
            }
        };

        // Store the exit status
        *exit_status = Some(code);
        code
    }
}
