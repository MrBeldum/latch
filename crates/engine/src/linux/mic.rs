//! A viewer's microphone as "Latch Microphone", an input any program on
//! this desktop can pick: a null sink to play into and a source remapped
//! from its monitor, loaded for the session and unloaded after it. All
//! over PulseAudio's native protocol, which PipeWire's pulse server also
//! speaks; the `pulseaudio` crate has no call for loading modules, so the
//! two commands go over a short connection of their own.

use anyhow::{bail, Context};
use futures::executor::block_on;
use latch_proto::control::AudioFormat;
use parking_lot::Mutex;
use pulseaudio::protocol::{
    self, Command, CommandReply, ProtocolError, TagStructRead, TagStructReader, TagStructWrite,
    TagStructWriter,
};
use pulseaudio::{AsPlaybackSource, Client, PlaybackStream};
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

const SINK: &str = "latch_mic_sink";
const SOURCE: &str = "latch_mic";
/// Queued before playing, again after running dry, and what a backlog is
/// cut back to: speech, not music, so a little delay buys no gaps.
const CUSHION_MS: usize = 20;
const MOST_MS: usize = 100;

/// `LoadModule` answers with the new module's index.
struct ModuleIndex(u32);
impl TagStructRead for ModuleIndex {
    fn read(ts: &mut TagStructReader<'_>, _: u16) -> Result<Self, ProtocolError> {
        Ok(Self(ts.read_u32()?))
    }
}
impl TagStructWrite for ModuleIndex {
    fn write(&self, w: &mut TagStructWriter<'_>, _: u16) -> Result<(), ProtocolError> {
        w.write_u32(self.0)
    }
}
impl CommandReply for ModuleIndex {}

/// A blocking connection for module commands.
struct Control {
    socket: BufReader<UnixStream>,
    version: u16,
    seq: u32,
}

impl Control {
    fn connect() -> anyhow::Result<Self> {
        let path = pulseaudio::socket_path_from_env().context("no PulseAudio socket")?;
        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();
        let socket = UnixStream::connect(path).context("connect to PulseAudio")?;
        let mut control = Self {
            socket: BufReader::new(socket),
            version: protocol::MAX_VERSION,
            seq: 0,
        };
        let auth: protocol::AuthReply = control.ask(Command::Auth(protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        }))?;
        control.version = protocol::MAX_VERSION.min(auth.version);
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::ApplicationName, c"Latch microphone");
        let _: protocol::SetClientNameReply = control.ask(Command::SetClientName(props))?;
        Ok(control)
    }

    fn send(&mut self, command: Command) -> anyhow::Result<()> {
        self.seq += 1;
        protocol::write_command_message(self.socket.get_mut(), self.seq, &command, self.version)?;
        Ok(())
    }

    fn ask<R: CommandReply>(&mut self, command: Command) -> anyhow::Result<R> {
        self.send(command)?;
        let (_, reply) = protocol::read_reply_message(&mut self.socket, self.version)?;
        Ok(reply)
    }

    fn load(&mut self, name: &str, arguments: &str) -> anyhow::Result<u32> {
        let reply: ModuleIndex = self
            .ask(Command::LoadModule(protocol::LoadModuleParams {
                name: CString::new(name)?,
                arguments: Some(CString::new(arguments)?),
            }))
            .with_context(|| format!("load {name}"))?;
        Ok(reply.0)
    }

    fn unload(&mut self, index: u32) -> anyhow::Result<()> {
        self.send(Command::UnloadModule(index))?;
        protocol::read_ack_message(&mut self.socket)?;
        Ok(())
    }
}

/// Played samples waiting for the server, with a small cushion.
#[derive(Default)]
struct Queue {
    samples: VecDeque<i16>,
    playing: bool,
}

/// What the session holds: dropping it ends the stream and unloads the
/// modules.
struct Loaded {
    modules: Vec<u32>,
    stream: Option<PlaybackStream>,
    _client: Client,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = block_on(stream.delete());
        }
        if let Ok(mut control) = Control::connect() {
            for index in self.modules.iter().rev() {
                let _ = control.unload(*index);
            }
        }
    }
}

pub(super) fn open(format: &AudioFormat) -> anyhow::Result<crate::mic::MicOut> {
    if format.channels != 1 || format.codec != "lpcm-rice" {
        bail!("unexpected microphone format {format:?}");
    }
    let client = Client::from_env(c"Latch microphone").context("connect to PulseAudio/PipeWire")?;
    // A host that died mid-session left its modules behind.
    for module in block_on(client.list_modules())? {
        let argument = module
            .argument
            .as_ref()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_default();
        if argument.contains(&format!("sink_name={SINK}"))
            || argument.contains(&format!("source_name={SOURCE}"))
        {
            let _ = Control::connect().and_then(|mut c| c.unload(module.index));
        }
    }
    let mut control = Control::connect()?;
    let mut loaded = Loaded {
        modules: Vec::new(),
        stream: None,
        _client: client,
    };
    loaded.modules.push(control.load(
        "module-null-sink",
        &format!(
            "sink_name={SINK} rate={} channels=1 sink_properties=device.description=Latch-microphone-feed",
            format.rate
        ),
    )?);
    loaded.modules.push(control.load(
        "module-remap-source",
        &format!(
            "master={SINK}.monitor source_name={SOURCE} source_properties='device.description=\"Latch Microphone\"'"
        ),
    )?);
    let rate = format.rate as usize;
    let cushion = rate * CUSHION_MS / 1000;
    let most = rate * MOST_MS / 1000;
    let queue = Arc::new(Mutex::new(Queue::default()));
    let params = protocol::PlaybackStreamParams {
        sample_spec: protocol::SampleSpec {
            format: protocol::SampleFormat::S16Le,
            channels: 1,
            sample_rate: format.rate,
        },
        channel_map: protocol::ChannelMap::mono(),
        sink_name: Some(CString::new(SINK)?),
        buffer_attr: protocol::stream::BufferAttr {
            max_length: u32::MAX,
            target_length: (cushion * 2) as u32,
            pre_buffering: 0,
            minimum_request_length: u32::MAX,
            fragment_size: u32::MAX,
        },
        flags: protocol::stream::StreamFlags {
            adjust_latency: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let source = {
        let queue = queue.clone();
        move |out: &mut [u8]| {
            let mut q = queue.lock();
            if !q.playing && q.samples.len() >= cushion {
                q.playing = true;
            }
            for pair in out.as_chunks_mut::<2>().0 {
                let sample = if q.playing {
                    q.samples.pop_front().unwrap_or_else(|| {
                        q.playing = false;
                        0
                    })
                } else {
                    0
                };
                pair.copy_from_slice(&sample.to_le_bytes());
            }
            // Zero would mean the end of the stream.
            out.len()
        }
    };
    loaded.stream = Some(block_on(
        loaded
            ._client
            .create_playback_stream(params, source.as_playback_source()),
    )?);
    let mut pcm = Vec::new();
    let mut last_len = format.frame_samples as usize;
    let push = move |frame: &[u8]| {
        let _ = &loaded;
        if frame.is_empty() {
            pcm.clear();
            pcm.resize(last_len, 0);
        } else if latch_codec::audio::decode_channels(frame, &mut pcm) != Ok(1) {
            return;
        }
        last_len = pcm.len();
        let mut q = queue.lock();
        q.samples.extend(&pcm);
        if q.samples.len() > most {
            let drop = q.samples.len() - cushion;
            q.samples.drain(..drop);
        }
    };
    Ok(crate::mic::MicOut {
        describe: "Latch Microphone (PulseAudio)".into(),
        push: Box::new(push),
    })
}
