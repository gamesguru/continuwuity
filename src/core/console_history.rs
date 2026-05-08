use std::{
	collections::VecDeque,
	fs::{File, OpenOptions},
	io::{BufRead, Write},
	path::PathBuf,
};

const HISTORY_LIMIT: usize = 1000;
const HISTORY_FILE: &str = ".c10y_history";

/// Persistent command history backed by a file, shared between console and
/// attach modes.
#[derive(Debug)]
pub struct ConsoleHistory {
	entries: VecDeque<String>,
	path: PathBuf,
}

impl ConsoleHistory {
	/// Load history from `~/.c10y_history`, creating the file if needed.
	#[must_use]
	pub fn new() -> Self {
		let path = std::env::var("HOME")
			.map_or_else(|_| PathBuf::from("."), PathBuf::from)
			.join(HISTORY_FILE);

		let mut entries = VecDeque::with_capacity(HISTORY_LIMIT);
		if let Ok(file) = File::open(&path) {
			for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
				if !line.is_empty() {
					entries.push_back(line);
				}
			}
			// Keep only last N entries in memory
			while entries.len() > HISTORY_LIMIT {
				entries.pop_front();
			}
		}

		Self { entries, path }
	}

	/// Add a line to the history and append it to the file.
	pub fn add(&mut self, line: &str) {
		if line.trim().is_empty() {
			return;
		}

		if self.entries.len() >= HISTORY_LIMIT {
			self.entries.pop_front();
		}
		self.entries.push_back(line.to_owned());

		// Append to persistent history file
		if let Ok(mut file) = OpenOptions::new()
			.create(true)
			.append(true)
			.open(&self.path)
		{
			_ = writeln!(file, "{line}");
		}
	}

	/// Iterate over history entries (oldest first).
	pub fn iter(&self) -> impl Iterator<Item = &String> { self.entries.iter() }

	/// Iterate over history entries (newest first).
	pub fn iter_rev(&self) -> impl Iterator<Item = &String> { self.entries.iter().rev() }

	/// Number of entries in history.
	#[must_use]
	pub fn len(&self) -> usize { self.entries.len() }

	/// Whether the history is empty.
	#[must_use]
	pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

impl Default for ConsoleHistory {
	fn default() -> Self { Self::new() }
}
