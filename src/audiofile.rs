use byteorder::{LittleEndian, ReadBytesExt};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::Cursor;

pub(crate) fn save_wav_file(input_bytes: Vec<u8>) {
	eprintln!("save_wav_file");
	// The sample rate (in Hz) of your audio.
	let sample_rate = 44100; // for example, CD quality

	// Assuming we have 1 channel (mono audio) and that `input_bytes` is a multiple of 4
	let num_samples = input_bytes.len() / 4;

	// Convert the Vec<u8> to Vec<f32>
	let mut cursor = Cursor::new(input_bytes);
	let mut samples = Vec::new();
	for _ in 0..num_samples {
		match cursor.read_i16::<LittleEndian>() {
			Ok(sample) => samples.push(sample),
			Err(e) => eprintln!("Error converting bytes to samples: {:?}", e),
		}
	}

	// Now write the samples to a WAV file using hound
	let spec = WavSpec {
		channels: 1,
		sample_rate: sample_rate as u32,
		bits_per_sample: 16,
		sample_format: SampleFormat::Int,
	};

	// Define a path for the intermediate WAV file
	let wav_path = "temp_audio.wav";

	let mut writer = WavWriter::create(wav_path, spec).expect("Failed to create WAV writer");

	// Write samples to the WAV writer
	for sample in samples {
		writer.write_sample(sample).expect("Failed to write sample");
	}

	// Finalize the WAV file
	writer.finalize().expect("Failed to finalize WAV file");

	eprintln!("convert to aac");

	let output_file = "temp_audio.aac"; // Replace with your desired output AAC file path
	let result = std::process::Command::new("ffmpeg")
		.args(&[
			"-y",
			"-i",
			wav_path, // Input file -c:a libfdk_aac -profile:a aac_he -b:a 64k
			"-c:a",
			"aac_at",
			"-vn",       // No video (audio-only)
			output_file, // Output file
		])
		.output()
		.expect("Failed to execute ffmpeg command");

	if result.status.success() {
		println!("Conversion completed successfully.");
	} else {
		eprintln!("Conversion failed.");
		eprintln!("stderr: {}", String::from_utf8_lossy(&result.stderr));
	}
	eprintln!("done");
}
