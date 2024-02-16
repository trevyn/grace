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
		.get("https://github.com/trevyn/scarlett/releases/latest/download/scarlett.zip")
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

	eprintln!("update available {}...", new_version);

	Ok(())
}
