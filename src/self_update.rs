use futures::stream::StreamExt;
use std::os::unix::fs::OpenOptionsExt;

#[tracked::tracked]
pub(crate) async fn self_update() -> Result<(), Box<dyn std::error::Error>> {
	if option_env!("BUILD_ID").is_none() {
		eprintln!("Running DEV; updates disabled.",);
		return Ok(());
	}

	eprintln!("checking for updates...");

	let res = reqwest::Client::builder()
		.redirect(reqwest::redirect::Policy::none())
		.build()?
		.get("https://github.com/trevyn/scarlett/releases/latest/download/scarlett")
		.send()
		.await?;

	if res.status() != 302 {
		Err(format!("Err, HTTP status {}, expected 302 redirect", res.status()))?;
	}
	let location = res.headers().get(reqwest::header::LOCATION)?.to_str()?;

	let new_version =
		regex::Regex::new(r"/releases/download/([a-z]+-[a-z]+)/")?.captures(location)?.get(1)?.as_str();

	if option_env!("BUILD_ID").unwrap_or_default() == new_version {
		eprintln!("Running latest! {}", new_version);
		return Ok(());
	}

	eprintln!("downloading update {}...", new_version);

	let res = reqwest::get(location).await?;
	let total_size: usize = res.content_length()?.try_into()?;

	if total_size < 10_000_000 {
		eprintln!(
			"Not updating; new release {} is unexpectedly small: {} bytes.",
			new_version, total_size
		);
		return Ok(());
	}

	// if total_size > 50_000_000 {
	// 	Err(format!(
	// 		"Not updating; new release {} is unexpectedly large: {} bytes.",
	// 		new_version, total_size
	// 	))?;
	// }

	let mut bytes = Vec::with_capacity(total_size);
	let mut stream = res.bytes_stream();

	while let Some(item) = stream.next().await {
		bytes.extend_from_slice(&item?);
		eprintln!(
			"downloading update {} {}% {}/{}...",
			new_version,
			bytes.len() * 100 / total_size,
			bytes.len(),
			total_size
		);
	}

	let bytes = bytes;

	if bytes.len() != total_size {
		eprintln!(
			"Not updating; downloaded incorrect number of bytes: {} of {}.",
			bytes.len(),
			total_size
		);
		return Ok(());
		// Err(format!(
		// 	"Not updating; downloaded incorrect number of bytes: {} of {}.",
		// 	bytes.len(),
		// 	total_size
		// ))?;
	}

	eprintln!("downloading update {} complete, {} bytes, saving to disk...", new_version, bytes.len());

	// yield format!(
	// 	"downloading update {} complete, {} bytes, saving to disk...",
	// 	new_version,
	// 	bytes.len()
	// );

	let current_exe = std::env::current_exe()?;
	let current_exe_cloned = current_exe.clone();
	let mut current_exe_update = current_exe.clone();
	current_exe_update.set_extension("update")?;
	let current_exe_update = current_exe_update;
	let bytes_len = bytes.len();

	tokio::task::spawn_blocking(move || -> Result<(), tracked::StringError> {
		let mut f =
			std::fs::OpenOptions::new().create(true).write(true).mode(0o700).open(&current_exe_update)?;
		std::io::Write::write_all(&mut f, &bytes)?;
		f.sync_all()?;
		std::fs::rename(current_exe_update, current_exe_cloned)?;
		Ok(())
	})
	.await??;
	Ok(())
}
