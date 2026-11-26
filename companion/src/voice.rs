//! Optional prerecorded companion cues. Playback receives only local PCM data.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::presentation::{self, Pack, Signals};

const MAX_WAV_BYTES: usize = 300 * 1024;
const PLAYER: &str = "/usr/bin/aplay";
const MIN_INTERVAL: Duration = Duration::from_millis(750);

struct Cue {
    rate: u32,
    pcm: Vec<u8>,
}

pub struct Voice {
    bindings: Vec<(String, Arc<Cue>)>,
    selected: Option<usize>,
    playback: Option<(mpsc::Sender<Option<Arc<Cue>>>, JoinHandle<()>)>,
}

impl Voice {
    pub fn silent() -> Self {
        Self {
            bindings: Vec::new(),
            selected: None,
            playback: None,
        }
    }

    pub fn load(pack: &Pack) -> Self {
        let mut voice = Self::silent();
        let mut cues = HashMap::<&str, Option<Arc<Cue>>>::new();
        for (key, entry) in &pack.manifest.voice {
            let Some(name) = entry.as_ref().and_then(|value| value.as_str()) else {
                continue;
            };
            if !presentation::valid_key(key) || (!cues.contains_key(name) && cues.len() >= 16) {
                continue;
            }
            let cue = cues.entry(name).or_insert_with(|| {
                let cue = pack.asset(name, ".wav", MAX_WAV_BYTES).ok().and_then(|bytes| decode_wave(&bytes));
                if cue.is_none() {
                    eprintln!("hypermesh-companion: unusable voice binding {key}; expected a short mono PCM WAV");
                }
                cue.map(Arc::new)
            });
            if let Some(cue) = cue {
                voice.bindings.push((key.clone(), Arc::clone(cue)));
            }
        }
        if !voice.bindings.is_empty() {
            let (send, receive) = mpsc::channel();
            match thread::Builder::new()
                .name("companion-voice".into())
                .spawn(move || play_cues(receive))
            {
                Ok(worker) => voice.playback = Some((send, worker)),
                Err(error) => eprintln!("hypermesh-companion: voice unavailable: {error}"),
            }
        }
        voice
    }

    pub fn update(&mut self, signals: &Signals<'_>) {
        let next = signals.select(&self.bindings);
        let cue = |index: usize| &self.bindings[index].1;
        let unchanged = match (self.selected, next) {
            (Some(previous), Some(next)) => Arc::ptr_eq(cue(previous), cue(next)),
            (None, None) => true,
            _ => false,
        };
        self.selected = next;
        if !unchanged {
            if let Some((send, _)) = &self.playback {
                let _ = send.send(next.map(|index| Arc::clone(cue(index))));
            }
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        if let Some((send, worker)) = self.playback.take() {
            drop(send);
            let _ = worker.join();
        }
    }
}

fn decode_wave(bytes: &[u8]) -> Option<Cue> {
    let size = u32_at(bytes, 4)? as usize;
    if bytes.get(..4)? != b"RIFF"
        || bytes.get(8..12)? != b"WAVE"
        || size.checked_add(8)? != bytes.len()
    {
        return None;
    }
    let mut rate = None;
    let mut pcm = None;
    let mut remaining = &bytes[12..];
    while !remaining.is_empty() {
        let size = u32_at(remaining, 4)? as usize;
        let end = 8usize.checked_add(size)?;
        let chunk = remaining.get(8..end)?;
        match remaining.get(..4)? {
            b"fmt " if rate.is_none() => {
                let sample_rate = u32_at(chunk, 4)?;
                if !(8_000..=48_000).contains(&sample_rate)
                    || chunk.get(..4)? != [1, 0, 1, 0]
                    || u32_at(chunk, 8)? != sample_rate * 2
                    || chunk.get(12..16)? != [2, 0, 16, 0]
                {
                    return None;
                }
                rate = Some(sample_rate);
            }
            b"data" if pcm.is_none() => pcm = Some(chunk),
            b"fmt " | b"data" => return None,
            _ => {}
        }
        remaining = remaining.get(end.checked_add(size % 2)?..)?;
    }
    let rate = rate?;
    let pcm = pcm?;
    if pcm.is_empty() || pcm.len() % 2 != 0 || pcm.len() > rate as usize * 2 * 3 {
        return None;
    }
    Some(Cue {
        rate,
        pcm: pcm.to_vec(),
    })
}

fn u32_at(bytes: &[u8], start: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(start..start + 4)?.try_into().ok()?,
    ))
}

fn play_cues(receive: mpsc::Receiver<Option<Arc<Cue>>>) {
    let mut playing: Option<Playing> = None;
    let mut pending = None;
    let mut ready_at = Instant::now();
    loop {
        match receive.recv_timeout(Duration::from_millis(50)) {
            Ok(first) => {
                playing = None;
                pending = receive.try_iter().last().unwrap_or(first);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if playing.as_mut().is_some_and(Playing::finished) {
            playing = None;
        }
        if Instant::now() >= ready_at {
            if let Some(cue) = pending.take() {
                match Playing::start(cue) {
                    Ok(cue) => playing = Some(cue),
                    Err(error) => {
                        eprintln!("hypermesh-companion: voice playback unavailable: {error}");
                        return;
                    }
                }
                ready_at = Instant::now() + MIN_INTERVAL;
            }
        }
    }
}

struct Playing {
    child: Child,
    writer: Option<JoinHandle<()>>,
    deadline: Instant,
}

impl Playing {
    fn start(cue: Arc<Cue>) -> std::io::Result<Self> {
        let mut child = Command::new(PLAYER)
            .args([
                "-q",
                "-t",
                "raw",
                "-f",
                "S16_LE",
                "-c",
                "1",
                "-r",
                &cue.rate.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut input = child.stdin.take().expect("piped audio");
        let mut playing = Self {
            child,
            writer: None,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        playing.writer = Some(thread::Builder::new().spawn(move || {
            let _ = input.write_all(&cue.pcm);
        })?);
        Ok(playing)
    }

    fn finished(&mut self) -> bool {
        Instant::now() >= self.deadline || !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_short_mono_wave_and_rejects_bad_headers_and_duration() {
        let wave = |samples: usize| {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&(36 + samples as u32 * 2).to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
            bytes.extend_from_slice(&16_000u32.to_le_bytes());
            bytes.extend_from_slice(&32_000u32.to_le_bytes());
            bytes.extend_from_slice(b"\x02\0\x10\0data");
            bytes.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
            bytes.resize(44 + samples * 2, 0);
            bytes
        };
        let mut bytes = wave(20);
        assert_eq!(decode_wave(&bytes).unwrap().pcm.len(), 40);
        assert!(decode_wave(&bytes[..bytes.len() - 1]).is_none());
        bytes[22] = 2;
        assert!(decode_wave(&bytes).is_none());
        assert!(decode_wave(&wave(48_001)).is_none());
    }
}
