use super::SampleData;
use turbosql::{execute, now_ms, select, update, Blob, Turbosql};

#[derive(Debug)]
pub(crate) struct Session {
	pub(crate) start_ms: i64,
	pub(crate) end_ms: i64,
}

impl Session {
	pub(crate) fn calculate_sessions() -> Vec<Self> {
		let mut sessions = Vec::new();

		let mut start_ms = 0;
		let mut prevv_record_ms = 0;

		select!(Vec<i64> "record_ms from sampledata where record_ms > 0 order by record_ms")
			.unwrap()
			.iter()
			.fold(None, |prev_record_ms, record_ms| {
				if let Some(prev_record_ms) = prev_record_ms {
					prevv_record_ms = prev_record_ms;
					if record_ms - prev_record_ms > 3000 {
						sessions.push(Session { start_ms, end_ms: prev_record_ms });
						start_ms = *record_ms;
					}
				} else {
					start_ms = *record_ms;
				}
				Some(*record_ms)
			});

		sessions.push(Session { start_ms, end_ms: prevv_record_ms });

		sessions
	}

	pub(crate) fn duration_ms(&self) -> i64 {
		self.end_ms - self.start_ms
	}

	pub(crate) fn samples(&self) -> Vec<u8> {
		select!(Vec<Blob> "sample_data from sampledata where record_ms >= ? and record_ms <= ? order by record_ms", self.start_ms, self.end_ms)
   .unwrap().iter().map(|blob| blob.to_vec()).flatten().collect()
	}
}
