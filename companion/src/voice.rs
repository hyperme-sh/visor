//! Optional prerecorded companion cues. Playback receives only local PCM data.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hypermesh_visor::companion_packs::{PcmWave as Cue, MAX_DISTINCT_VOICES};

use crate::presentation::{self, Pack, Signals};

const PLAYER: &str = "/usr/bin/aplay";
const MIN_INTERVAL: Duration = Duration::from_millis(750);

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
            if !presentation::valid_key(key) || (!cues.contains_key(name) && cues.len() >= MAX_DISTINCT_VOICES) {
                continue;
            }
            let cue = cues.entry(name).or_insert_with(|| {
                let cue = pack.voice(name);
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
