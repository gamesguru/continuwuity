use conduwuit_core::{Config, Result, error::Error};
use rustyline_async::{Readline, ReadlineEvent};
use tokio::
{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	net::UnixStream,
};

use crate::clap::{Args, update};

pub(crate) fn run(args: &Args) -> Result<()> {
	let mut config_paths = args.config.clone().unwrap_or_default();
	if config_paths.is_empty() {
		config_paths.push("conduwuit.toml".into());
	}

	let config = Config::load(&config_paths)
		.and_then(|raw| update(raw, args))
		.and_then(|raw| Config::new(&raw))?;

	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|e| {
			eprintln!("Failed to initialize tokio runtime: {e:?}");
			Error::bad_database("Failed to initialize tokio runtime")
		})?;

	runtime.block_on(async_run(&config))
}

async fn async_run(config: &Config) -> Result<()> {
	let socket_path = config.database_path.join("console.sock");

	let mut stream = match UnixStream::connect(&socket_path).await {
		| Ok(s) => s,
		| Err(e) => {
			eprintln!("Failed to connect to console socket at {}: {e}", socket_path.display());
			eprintln!("Is the conduwuit server currently running?");
			return Err(Error::bad_database("Failed to connect to server"));
		},
	};

	// We don't have a conduwuit instance here, so we can't use
	// `conduwuit_core::Log`, we don't have any logs anyway!

	println!("Connected to conduwuit admin console at {}", socket_path.display());
	println!("Type \"help\" for help, ^D or `Quit` to exit.");

	let mut stream_reader = BufReader::new(&mut stream);
	let mut response_buf = Vec::new();

	let (mut readline, mut writer) = Readline::new("uwu> ".to_owned()).map_err(|e| {
		eprintln!("Failed to initialize readline: {e:?}");
		Error::bad_database("Failed to initialize readline")
	})?;

	readline.set_tab_completer(conduwuit_admin::complete);

	loop {
		let event = readline.readline().await;
		match event {
			| Ok(ReadlineEvent::Line(line)) => {
				let trimmed = line.trim();
				if trimmed.is_empty() {
					continue;
				}

				// Local client-side exit just drops the socket
				if trimmed.eq_ignore_ascii_case("quit") {
					break;
				}

				_ = readline.add_history_entry(line.clone());

				// Clear the prompt line before sending to server and waiting
				// This ensures job output starts at the beginning of the line
				// and the prompt doesn't look "hung" while the server processes.
				std::io::Write::write_all(&mut writer, b"\r\x1b[K").ok();

				// Send line to server
				if let Err(_e) = stream_reader.get_mut().write_all(line.as_bytes()).await {
					std::io::Write::write_all(
						&mut writer,
						b"Failed to write to socket\r\n",
					)
					.ok();
					break;
				}
				if let Err(_e) = stream_reader.get_mut().write_all(b"\n").await {
					std::io::Write::write_all(
						&mut writer,
						b"Failed to write to socket\r\n",
					)
					.ok();
					break;
				}

				// Await response from server OR Ctrl+C
				response_buf.clear();
				tokio::select! {
					res = stream_reader.read_until(b'\0', &mut response_buf) => {
							match res {
								| Ok(0) => {
											std::io::Write::write_all(&mut writer, b"Server disconnected.\r\n").ok();
											break;
								},
								| Ok(_) => {
											if response_buf.ends_with(b"\0") {
													response_buf.pop();
										}
										let response_str = String::from_utf8_lossy(&response_buf);
										if !response_str.is_empty() {
													let formatted = conduwuit_service::admin::console::format(&response_str);
													std::io::Write::write_all(&mut writer, formatted.as_bytes()).ok();
										}
										},
									| Err(_e) => {
											std::io::Write::write_all(&mut writer, b"Failed to read from socket\r\n").ok();
											break;
									}
								}
						},
					_ = tokio::signal::ctrl_c() => {
							std::io::Write::write_all(&mut writer, b"Interrupted.\r\n").ok();
							// Drop stream and reconnect to cancel server job
							let new_stream = match UnixStream::connect(&socket_path).await {
								| Ok(s) => s,
								| Err(_e) => {
									eprintln!("Failed to reconnect to console socket");
									break;
								}
						};
						stream = new_stream;
						stream_reader = BufReader::new(&mut stream);
					}
				}
			},
			| Ok(ReadlineEvent::Interrupted) => continue,
			| Ok(ReadlineEvent::Eof | ReadlineEvent::Quit) => break,
			| Err(_e) => {
				std::io::Write::write_all(
					&mut writer,
					b"Console read error\r\n",
				)
				.ok();
				break;
			},
		}
	}

	Ok(())
}
