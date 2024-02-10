use byteorder::{LittleEndian, ReadBytesExt};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::Cursor;

pub(crate) fn save_wav_file(input_bytes: Vec<u8>) {
	// The sample rate (in Hz) of your audio.
	let sample_rate = 44100; // for example, CD quality

	// Assuming we have 1 channel (mono audio) and that `input_bytes` is a multiple of 4
	let num_samples = input_bytes.len() / 4;

	// Convert the Vec<u8> to Vec<f32>
	let mut cursor = Cursor::new(input_bytes);
	let mut samples = Vec::new();
	for _ in 0..num_samples {
		match cursor.read_f32::<LittleEndian>() {
			Ok(sample) => samples.push(sample),
			Err(e) => eprintln!("Error converting bytes to f32 samples: {:?}", e),
		}
	}

	// Now write the samples to a WAV file using hound
	let spec = WavSpec {
		channels: 1,
		sample_rate: sample_rate as u32,
		bits_per_sample: 32,
		sample_format: SampleFormat::Float,
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
}
