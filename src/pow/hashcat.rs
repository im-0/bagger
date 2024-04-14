use core::fmt;
use std::{
    fmt::{Display, Formatter},
    fs::File,
    io::{BufWriter, Write},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, ensure, Context, Result};
use futures::Future;
use linked_hash_set::LinkedHashSet;
use solana_sdk::keccak;
use solana_sdk::pubkey::Pubkey;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    select,
    sync::mpsc::{self, error::TryRecvError},
    try_join,
};
use tracing::{debug, info, trace};

use crate::{cli::PoWHashcatOptions, signals::ExitFlag};

use super::{
    server::GetServerImplFnBoxed,
    types::{NextHash, PoWRequest, PoWResponse},
};

const MAX_REQUESTS_BATCH_SIZE: usize = 256;

pub fn get_hashcat_pow_server_impl(
    options: PoWHashcatOptions,
    exit_flag: ExitFlag,
) -> GetServerImplFnBoxed {
    Box::new(move |requests_receiver, results_sender| {
        Box::pin(run_hashcat_pow(
            requests_receiver,
            results_sender,
            options.clone(),
            exit_flag.clone(),
        ))
    })
}

async fn run_hashcat_pow(
    mut requests_receiver: mpsc::Receiver<Vec<PoWRequest>>,
    results_sender: mpsc::Sender<PoWResponse>,
    options: PoWHashcatOptions,
    exit_flag: ExitFlag,
) -> Result<()> {
    let hashes_tmp = tempfile::Builder::new()
        .prefix("bagger-hashcat-")
        .suffix(".hashes")
        .tempfile()
        .context("Unable to create file for hashes")?;
    let hashes_path = hashes_tmp
        .path()
        .to_str()
        .ok_or_else(|| anyhow!("Unable to convert path to string: {:?}", hashes_tmp.path()))?
        .to_string();

    info!(
        "Using {} as data directory and {} as hashes file for hashcat",
        options.data_directory, hashes_path
    );

    let mut command_args = vec![
        "--markov-disable".to_string(),
        "--attack-mode".to_string(),
        "3".to_string(), // brute-force
        "--outfile-format".to_string(),
        "1,3,5".to_string(), // hash, hex_plain, timestamp relative
        "--hash-type".to_string(),
        options.module,
        "--machine-readable".to_string(),
        "--potfile-disable".to_string(),
    ];
    command_args.extend(options.hashcat_args);
    command_args.extend([hashes_path.clone(), "?l?b?b?b?b?b?b?b".to_string()]);

    'main: loop {
        let mut requests = match requests_receiver.recv().await {
            Some(requests) => requests,

            None => {
                debug!("Requests sender closed");
                break;
            }
        };
        // Try to read more requests.
        while requests.len() < MAX_REQUESTS_BATCH_SIZE {
            let more_requests = match requests_receiver.try_recv() {
                Ok(more_requests) => more_requests,
                Err(TryRecvError::Empty) => break,

                Err(TryRecvError::Disconnected) => {
                    debug!("Requests sender closed");
                    break 'main;
                }
            };

            requests.extend(more_requests);
        }
        info!("Got {} requests", requests.len());

        // Remove duplicates.
        let requests: LinkedHashSet<_> = LinkedHashSet::from_iter(requests.into_iter());
        let requests = Vec::from_iter(requests.into_iter());
        info!("{} requests after duplicates removed", requests.len());

        // Write hashes into file.
        let mut hashes_file = File::create(&hashes_path)
            .with_context(|| format!("Unable to create {}", hashes_path))?;
        let mut writer = BufWriter::new(&mut hashes_file);
        for request in &requests {
            write_pow_request(&mut writer, request)
                .with_context(|| format!("Unable to write hash into {}", hashes_path))?;
        }
        writer
            .flush()
            .with_context(|| format!("Unable to flush {}", hashes_path))?;

        // Run hashcat.
        let num_responses = run_hashcat(
            &results_sender,
            &options.command,
            &command_args,
            &options.data_directory,
            exit_flag.clone(),
        )
        .await?;

        ensure!(
            requests.len() == num_responses,
            "Expected {} hashes from hashcat, but got only {}",
            requests.len(),
            num_responses,
        );
    }

    Ok(())
}

fn write_pow_request(writer: &mut BufWriter<&mut File>, request: &PoWRequest) -> Result<()> {
    writer.write_all(hex::encode(request.difficulty.as_ref()).as_bytes())?;
    writer.write_all(b":")?;
    writer.write_all(hex::encode(request.prev_hash.as_ref()).as_bytes())?;
    writer.write_all(hex::encode(request.signer.as_ref()).as_bytes())?;
    writer.write_all(b"\n")?;

    Ok(())
}

async fn run_hashcat(
    results_sender: &mpsc::Sender<PoWResponse>,
    hashcat: &str,
    command_args: &[String],
    data_dir: &str,
    mut exit_flag: ExitFlag,
) -> Result<usize> {
    info!("Starting {} with arguments {:?}", hashcat, command_args);
    let mut command = Command::new(hashcat)
        .args(command_args)
        .current_dir(data_dir)
        .env("PWD", data_dir)
        .env("HOME", data_dir)
        .env("XDG_DATA_HOME", data_dir)
        .env("XDG_CACHE_HOME", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Unable to run hashcat")?;

    // Close stdin.
    let _ = command
        .stdin
        .take()
        .expect("Logic error: stdin should be there becauese of configuration");

    let stdout = command
        .stdout
        .take()
        .expect("Logic error: stdout should be there becauese of configuration");

    let results_sender = results_sender.clone();
    let num_responses = Arc::new(AtomicUsize::new(0));
    let orig_num_responses = num_responses.clone();

    // TODO: Remove this hack.
    let prev_timestamp = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Logic error: unable to get current time")
            .as_secs(),
    ));

    let line_check = move |line: String| {
        let results_sender = results_sender.clone();
        let num_responses = num_responses.clone();
        let prev_timestamp = prev_timestamp.clone();
        async move {
            if let Some(response_or_err) = find_response(&line) {
                match response_or_err {
                    Ok(response) => {
                        // Fix elapsed time.
                        let new_timestamp = response.2.as_secs();
                        let response = (
                            response.0,
                            response.1,
                            Duration::from_secs(
                                new_timestamp - prev_timestamp.load(Ordering::Relaxed),
                            ),
                        );
                        prev_timestamp.store(new_timestamp, Ordering::Relaxed);

                        let _ = num_responses.fetch_add(1, Ordering::Relaxed);
                        if results_sender.send(response).await.is_err() {
                            debug!("Results receiver closed");
                            // TODO: Break the loop here and exit.
                        }
                        Ok(())
                    }

                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            }
        }
    };
    let stdout_reader = tokio::spawn({
        let mut exit_flag = exit_flag.clone();

        async move {
            select! {
                r = exit_flag.wait() => r,
                r = line_reader(stdout, StreamType::Out, line_check) => r,
            }
        }
    });

    let stderr = command
        .stderr
        .take()
        .expect("Logic error: stderr should be there becauese of configuration");
    let stderr_reader = tokio::spawn(async move {
        select! {
            r = exit_flag.wait() => r,
            r = line_reader(stderr, StreamType::Error, |_| async { Ok(()) }) => r,
        }
    });

    let exit_status = try_join!(
        async {
            command
                .wait()
                .await
                .context("Unable to spawn hashcat process")
        },
        async {
            stdout_reader
                .await
                .context("Unable to join stdout reading thread")
        },
        async {
            stderr_reader
                .await
                .context("Unable to join stderr reading thread")
        },
    )?
    .0;

    ensure!(
        exit_status.code() == Some(0),
        "hashcat exited with code {}",
        exit_status
    );

    Ok(orig_num_responses.load(Ordering::Relaxed))
}

async fn line_reader<R, LCF, Fut>(
    stream: R,
    stream_type: StreamType,
    line_check_fn: LCF,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    LCF: Fn(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut lines = BufReader::new(stream).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .with_context(|| format!("Unable to read line from hascat's {}", stream_type.name()))?
    {
        for line in line.split('\r') {
            let line = line.trim_end();
            info!("{} {}", stream_type, line);

            // TODO: Fight borrow checker for the privilege to pass &str here.
            line_check_fn(line.to_string()).await?;
        }
    }

    Ok(())
}

fn find_response(line: &str) -> Option<Result<PoWResponse>> {
    let parts: Vec<_> = line.trim().split(':').collect();

    // Timestamp, difficulty, previous hash + signer, nonce.
    if parts.len() != 4 {
        return None;
    }
    trace!("Possible response line: {}", line);

    // Timestamp.
    let timestamp = Duration::from_secs(parts[0].parse::<u64>().ok()?);

    // Difficulty.
    if parts[1].len() != 64 {
        return None;
    }
    let mut difficulty = [0; 32];
    hex::decode_to_slice(parts[1], &mut difficulty).ok()?;
    let difficulty = keccak::Hash::new(&difficulty);

    // Previous hash and signer.
    if parts[2].len() != 128 {
        return None;
    }

    let mut prev_hash = [0; 32];
    hex::decode_to_slice(&parts[2][..64], &mut prev_hash).ok()?;
    let prev_hash = keccak::Hash::new(&prev_hash);

    let mut signer = [0; 32];
    hex::decode_to_slice(&parts[2][64..], &mut signer).ok()?;
    let signer = Pubkey::new_from_array(signer);

    // Nonce.
    if parts[3].len() != 16 {
        return None;
    }
    let mut nonce_bytes = [0; 8];
    hex::decode_to_slice(parts[3], &mut nonce_bytes).ok()?;
    let nonce = u64::from_le_bytes(nonce_bytes);

    // Calculate and verify next hash.
    let next_hash = keccak::hashv(&[prev_hash.as_ref(), signer.as_ref(), &nonce_bytes]);
    if next_hash.gt(&difficulty) {
        return Some(Err(anyhow!(
            "hashcat generated wrong nonce value: {}",
            line
        )));
    }

    Some(Ok((
        PoWRequest {
            difficulty,
            prev_hash,
            signer,
        },
        NextHash { next_hash, nonce },
        timestamp,
    )))
}

enum StreamType {
    Out,
    Error,
}

impl StreamType {
    const fn name(&self) -> &'static str {
        match self {
            Self::Out => "stdout",
            Self::Error => "stderr",
        }
    }
}

impl Display for StreamType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Out => write!(f, "[O]"),
            Self::Error => write!(f, "[E]"),
        }
    }
}
