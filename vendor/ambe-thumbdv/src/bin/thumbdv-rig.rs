//! thumbdv-rig — probe and golden-vector capture for ThumbDV (AMBE-3000).
//!
//! Commands:
//!   probe [--port PATH] [--baud N] — detect device, print metadata + timing
//!   enc <in.wav> <out.ambe> [--port PATH] — encode WAV to AMBE frames
//!   dec <in.ambe> <out.wav> [--port PATH] — decode AMBE frames to WAV

use ambe_thumbdv::{detect, DeviceError, SerialTransport, ThumbDv, FRAME_BYTES, FRAME_SAMPLES};
use std::f32;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: thumbdv-rig COMMAND [ARGS]

Commands:
  probe [--port PATH] [--baud N]      Detect device and print metadata + timing
  enc <in.wav> <out.ambe> [--port PATH]   Encode 8 kHz mono s16 WAV to AMBE frames
  dec <in.ambe> <out.wav> [--port PATH]   Decode AMBE frames to 8 kHz mono s16 WAV";

/// Parsed command line.
enum Cmd {
    Probe {
        port: Option<String>,
        baud: Option<u32>,
    },
    Enc {
        input: PathBuf,
        output: PathBuf,
        port: Option<String>,
    },
    Dec {
        input: PathBuf,
        output: PathBuf,
        port: Option<String>,
    },
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    match args.first().map(String::as_str) {
        Some("probe") => {
            let mut port = None;
            let mut baud = None;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                if arg == "--port" {
                    port = Some(it.next().ok_or("--port needs a value")?.clone());
                } else if arg == "--baud" {
                    let v = it.next().ok_or("--baud needs a value")?;
                    baud = Some(
                        v.parse()
                            .map_err(|_| format!("--baud: not a number: {v}"))?,
                    );
                } else if arg.starts_with("--") {
                    return Err(format!("unknown option {arg}"));
                } else {
                    return Err("probe takes only --port and --baud".into());
                }
            }
            Ok(Cmd::Probe { port, baud })
        }
        Some("enc") => {
            let mut positional = Vec::new();
            let mut port = None;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                if arg == "--port" {
                    port = Some(it.next().ok_or("--port needs a value")?.clone());
                } else if arg.starts_with("--") {
                    return Err(format!("unknown option {arg}"));
                } else {
                    positional.push(arg.clone());
                }
            }
            if positional.len() != 2 {
                return Err("enc needs exactly <input> and <output>".into());
            }
            Ok(Cmd::Enc {
                input: PathBuf::from(&positional[0]),
                output: PathBuf::from(&positional[1]),
                port,
            })
        }
        Some("dec") => {
            let mut positional = Vec::new();
            let mut port = None;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                if arg == "--port" {
                    port = Some(it.next().ok_or("--port needs a value")?.clone());
                } else if arg.starts_with("--") {
                    return Err(format!("unknown option {arg}"));
                } else {
                    positional.push(arg.clone());
                }
            }
            if positional.len() != 2 {
                return Err("dec needs exactly <input> and <output>".into());
            }
            Ok(Cmd::Dec {
                input: PathBuf::from(&positional[0]),
                output: PathBuf::from(&positional[1]),
                port,
            })
        }
        Some(other) => Err(format!("unknown subcommand {other}")),
        None => Err("missing subcommand".into()),
    }
}

/// Connect to device and return (port, baud, device).
/// Per ambe3000-protocol.md §8.1: connection helper for probe/enc/dec.
/// If --port given, try given baud (or 460800/230400 fallback per §6);
/// else call detect() (port from detect() is unavailable; see FIXME below).
fn connect_device(
    port: Option<&str>,
    baud: Option<u32>,
) -> Result<(String, u32, ThumbDv<SerialTransport>), DeviceError> {
    if let Some(p) = port {
        // Explicit port: try given baud or fallback to 460800 then 230400.
        let bauds = if let Some(b) = baud {
            vec![b]
        } else {
            vec![460800, 230400]
        };
        for b in bauds {
            match SerialTransport::open(p, b) {
                Ok(transport) => match ThumbDv::init_with(transport) {
                    Ok(dev) => return Ok((p.to_string(), b, dev)),
                    Err(e) if baud.is_some() => return Err(e), // fail fast if baud was explicit
                    Err(_) => continue,                        // try next baud
                },
                Err(e) if baud.is_some() => return Err(DeviceError::Io(e)), // fail fast
                Err(_) => continue,
            }
        }
        Err(DeviceError::Timeout("failed to connect on explicit port"))
    } else {
        // Auto-detect: detect() returns device but not the port/baud that succeeded.
        // FIXME: device.rs detect() should return (port: String, baud: u32, device)
        // to fully satisfy protocol doc §9 item 3 (VERIFY-HW).
        let dev = detect()?;
        let candidates = SerialTransport::candidate_ports();
        let port_str = if candidates.is_empty() {
            "<no candidates found>".to_string()
        } else {
            format!("<candidates: {}>", candidates.join(", "))
        };
        // Baud is unknown after auto-detect (tried 460800 then 230400 per §6).
        Ok((port_str, 0, dev))
    }
}

/// Run the probe command.
/// Per ambe3000-protocol.md §9 items 3, 4, 8 (VERIFY-HW): print connection
/// parameters, device metadata, and measure encode latency.
fn cmd_probe(port: Option<&str>, baud: Option<u32>) -> Result<(), DeviceError> {
    let (actual_port, actual_baud, mut dev) = connect_device(port, baud)?;

    println!("Port: {}", actual_port);
    if actual_baud != 0 {
        println!("Baud: {}", actual_baud);
    }

    println!("PRODID: {}", dev.prodid());
    println!("VERSTRING: {}", dev.version());

    let getcfg = dev.getcfg()?;
    print!("GETCFG: ");
    for b in getcfg {
        print!("{:02X} ", b);
    }
    println!();

    let readcfg = dev.readcfg()?;
    print!("READCFG: ");
    for b in readcfg {
        print!("{:02X} ", b);
    }
    println!();

    // Timing sample: 10 lockstep encode round-trips of a real 1 kHz sine.
    // Amplitude 8000, sample rate 8000 Hz, duration 160 samples = 20 ms.
    let mut latencies = Vec::new();
    let tone_frame: [i16; FRAME_SAMPLES] = std::array::from_fn(|i| {
        (8000.0 * (2.0 * f32::consts::PI * 1000.0 * i as f32 / 8000.0).sin()) as i16
    });

    for _ in 0..10 {
        let start = Instant::now();
        let _frame = dev.encode_frame(&tone_frame)?;
        let elapsed = start.elapsed();
        latencies.push(elapsed.as_micros() as u64);
    }

    if !latencies.is_empty() {
        let min = latencies.iter().min().unwrap();
        let max = latencies.iter().max().unwrap();
        let mean = latencies.iter().sum::<u64>() / latencies.len() as u64;
        println!(
            "Encode latency (µs): min={}, mean={}, max={}",
            min, mean, max
        );
    }

    Ok(())
}

/// Run the enc command.
/// Per ambe3000-protocol.md §8.2 (cookbook): reinit device, encode each frame lockstep,
/// write concatenated 9-byte frames.
fn cmd_enc(input: &PathBuf, output: &PathBuf, port: Option<&str>) -> Result<(), DeviceError> {
    let (_port, _baud, mut dev) = connect_device(port, None)?;
    dev.reinit()?;

    // Read WAV file.
    let samples = wav::read_wav_s16_mono_8k(input)
        .map_err(|e| DeviceError::Protocol(format!("WAV read error: {}", e)))?;

    // Open output file.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output)
        .map_err(DeviceError::Io)?;
    let mut writer = BufWriter::new(file);

    // Encode frames.
    let frame_count = samples.len().div_ceil(FRAME_SAMPLES);
    for (i, chunk) in samples.chunks(FRAME_SAMPLES).enumerate() {
        // Zero-pad tail.
        let mut frame = [0i16; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);

        let encoded = dev.encode_frame(&frame)?;
        writer.write_all(&encoded).map_err(DeviceError::Io)?;

        if (i + 1) % 100 == 0 {
            eprintln!("Encoded {} frames", i + 1);
        }
    }

    println!("{} frames: {}", frame_count, dev.version());
    Ok(())
}

/// Run the dec command.
/// Per ambe3000-protocol.md §8.3 (cookbook): reinit device, decode each frame lockstep,
/// collect PCM samples and write WAV.
fn cmd_dec(input: &PathBuf, output: &PathBuf, port: Option<&str>) -> Result<(), DeviceError> {
    let (_port, _baud, mut dev) = connect_device(port, None)?;
    dev.reinit()?;

    // Read AMBE frames.
    let file = File::open(input).map_err(DeviceError::Io)?;
    let mut reader = BufReader::new(file);
    let mut frame_data = Vec::new();
    reader
        .read_to_end(&mut frame_data)
        .map_err(DeviceError::Io)?;

    if frame_data.len() % FRAME_BYTES != 0 {
        return Err(DeviceError::Protocol(format!(
            "AMBE file size {} is not a multiple of {}",
            frame_data.len(),
            FRAME_BYTES
        )));
    }

    let frame_count = frame_data.len() / FRAME_BYTES;
    let mut samples = Vec::new();

    // Decode frames.
    for (i, chunk) in frame_data.chunks(FRAME_BYTES).enumerate() {
        let mut frame = [0u8; FRAME_BYTES];
        frame.copy_from_slice(chunk);

        let pcm = dev.decode_frame(&frame)?;
        samples.extend_from_slice(&pcm);

        if (i + 1) % 100 == 0 {
            eprintln!("Decoded {} frames", i + 1);
        }
    }

    // Write WAV file.
    wav::write_wav_s16_mono_8k(output, &samples)
        .map_err(|e| DeviceError::Protocol(format!("WAV write error: {}", e)))?;

    println!("{} frames: {}", frame_count, dev.version());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_args(&args) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = match cmd {
        Cmd::Probe { port, baud } => cmd_probe(port.as_deref(), baud),
        Cmd::Enc {
            input,
            output,
            port,
        } => cmd_enc(&input, &output, port.as_deref()),
        Cmd::Dec {
            input,
            output,
            port,
        } => cmd_dec(&input, &output, port.as_deref()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

// Minimal local wav module (~70 lines): strict reader and canonical writer.
mod wav {
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Path;

    /// Strictly read RIFF/PCM16/mono/8k WAV file, skip unknown chunks, return samples.
    pub fn read_wav_s16_mono_8k<P: AsRef<Path>>(path: P) -> io::Result<Vec<i16>> {
        let mut file = File::open(path)?;
        let mut buf = [0u8; 12];
        file.read_exact(&mut buf)?;

        // Check RIFF header.
        if &buf[0..4] != b"RIFF" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a RIFF file",
            ));
        }
        if &buf[8..12] != b"WAVE" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a WAVE file",
            ));
        }

        // Scan for fmt chunk.
        let mut fmt_found = false;

        loop {
            let mut chunk_hdr = [0u8; 8];
            if file.read_exact(&mut chunk_hdr).is_err() {
                break;
            }

            let chunk_id = &chunk_hdr[0..4];
            let chunk_size =
                u32::from_le_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]])
                    as usize;

            if chunk_id == b"fmt " {
                if chunk_size < 16 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fmt chunk too small",
                    ));
                }
                let mut fmt = [0u8; 16];
                file.read_exact(&mut fmt)?;

                let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
                if audio_format != 1 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "not PCM format"));
                }

                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                if channels != 1 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "not mono"));
                }

                let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                if sample_rate != 8000 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sample rate not 8000 Hz",
                    ));
                }

                let bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);
                if bits_per_sample != 16 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "not 16-bit samples",
                    ));
                }

                fmt_found = true;

                // Skip rest of fmt chunk if larger than 16.
                if chunk_size > 16 {
                    file.seek(SeekFrom::Current((chunk_size - 16) as i64))?;
                }
            } else if chunk_id == b"data" {
                if !fmt_found {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "data chunk before fmt chunk",
                    ));
                }

                // Read PCM data.
                let mut data = vec![0u8; chunk_size];
                file.read_exact(&mut data)?;

                let mut samples = Vec::new();
                for i in (0..data.len()).step_by(2) {
                    if i + 1 < data.len() {
                        let s = i16::from_le_bytes([data[i], data[i + 1]]);
                        samples.push(s);
                    }
                }
                return Ok(samples);
            } else {
                // Skip unknown chunk.
                file.seek(SeekFrom::Current(chunk_size as i64))?;
            }
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no data chunk found",
        ))
    }

    /// Write 44-byte canonical WAV header + s16 LE samples.
    pub fn write_wav_s16_mono_8k<P: AsRef<Path>>(path: P, samples: &[i16]) -> io::Result<()> {
        let mut file = File::create(path)?;
        let data_len = (samples.len() * 2) as u32;

        file.write_all(b"RIFF")?;
        file.write_all(&(36 + data_len).to_le_bytes())?;
        file.write_all(b"WAVE")?;
        file.write_all(b"fmt ")?;
        file.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
        file.write_all(&1u16.to_le_bytes())?; // audio format: PCM
        file.write_all(&1u16.to_le_bytes())?; // channels: mono
        file.write_all(&8000u32.to_le_bytes())?; // sample rate
        file.write_all(&16000u32.to_le_bytes())?; // byte rate
        file.write_all(&2u16.to_le_bytes())?; // block align
        file.write_all(&16u16.to_le_bytes())?; // bits per sample
        file.write_all(b"data")?;
        file.write_all(&data_len.to_le_bytes())?;

        for s in samples {
            file.write_all(&s.to_le_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn unique_temp_path() -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let filename = format!("thumbdv_test_{}_{}.wav", pid, counter);
        std::env::temp_dir().join(filename)
    }

    #[test]
    fn parse_probe_command() {
        match parse_args(&args(&["probe"])).unwrap() {
            Cmd::Probe {
                port: None,
                baud: None,
            } => {}
            _ => panic!("expected probe with no args"),
        }

        match parse_args(&args(&["probe", "--port", "/dev/ttyUSB0"])).unwrap() {
            Cmd::Probe {
                port: Some(p),
                baud: None,
            } => assert_eq!(p, "/dev/ttyUSB0"),
            _ => panic!("expected probe with port"),
        }

        match parse_args(&args(&["probe", "--baud", "230400"])).unwrap() {
            Cmd::Probe {
                port: None,
                baud: Some(b),
            } => assert_eq!(b, 230400),
            _ => panic!("expected probe with baud"),
        }
    }

    #[test]
    fn parse_enc_command() {
        match parse_args(&args(&["enc", "in.wav", "out.ambe"])).unwrap() {
            Cmd::Enc {
                input,
                output,
                port: None,
            } => {
                assert_eq!(input.to_str().unwrap(), "in.wav");
                assert_eq!(output.to_str().unwrap(), "out.ambe");
            }
            _ => panic!("expected enc with input/output"),
        }

        match parse_args(&args(&[
            "enc",
            "in.wav",
            "out.ambe",
            "--port",
            "/dev/ttyUSB0",
        ]))
        .unwrap()
        {
            Cmd::Enc { port: Some(p), .. } => assert_eq!(p, "/dev/ttyUSB0"),
            _ => panic!("expected enc with port"),
        }
    }

    #[test]
    fn parse_dec_command() {
        match parse_args(&args(&["dec", "in.ambe", "out.wav"])).unwrap() {
            Cmd::Dec {
                input,
                output,
                port: None,
            } => {
                assert_eq!(input.to_str().unwrap(), "in.ambe");
                assert_eq!(output.to_str().unwrap(), "out.wav");
            }
            _ => panic!("expected dec with input/output"),
        }
    }

    #[test]
    fn reject_unknown_commands() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["unknown"])).is_err());
    }

    #[test]
    fn reject_bad_probe_args() {
        assert!(parse_args(&args(&["probe", "extra"])).is_err());
        assert!(parse_args(&args(&["probe", "--bogus"])).is_err());
        assert!(parse_args(&args(&["probe", "--port"])).is_err()); // --port with no value
    }

    #[test]
    fn reject_bad_enc_args() {
        assert!(parse_args(&args(&["enc"])).is_err());
        assert!(parse_args(&args(&["enc", "only_one"])).is_err());
        assert!(parse_args(&args(&["enc", "in", "out", "--bogus"])).is_err());
    }

    #[test]
    fn reject_bad_dec_args() {
        assert!(parse_args(&args(&["dec"])).is_err());
        assert!(parse_args(&args(&["dec", "only_one"])).is_err());
        assert!(parse_args(&args(&["dec", "in", "out", "--unknown"])).is_err());
    }

    #[test]
    fn wav_write_read_round_trip() {
        let path = unique_temp_path();

        let original = vec![0i16, 1000, -1000, 32767, -32768];
        wav::write_wav_s16_mono_8k(&path, &original).unwrap();

        let read_back = wav::read_wav_s16_mono_8k(&path).unwrap();
        assert_eq!(read_back, original);

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_reject_stereo() {
        let path = unique_temp_path();
        let mut file = File::create(&path).unwrap();

        // Write a stereo WAV header (channels = 2).
        file.write_all(b"RIFF").unwrap();
        file.write_all(&44u32.to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&2u16.to_le_bytes()).unwrap(); // STEREO
        file.write_all(&8000u32.to_le_bytes()).unwrap(); // sample rate
        file.write_all(&32000u32.to_le_bytes()).unwrap(); // byte rate
        file.write_all(&4u16.to_le_bytes()).unwrap(); // block align
        file.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
        file.write_all(b"data").unwrap();
        file.write_all(&0u32.to_le_bytes()).unwrap();
        drop(file);

        let result = wav::read_wav_s16_mono_8k(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mono"));

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }
}
