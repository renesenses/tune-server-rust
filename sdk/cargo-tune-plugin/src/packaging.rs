use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
use tune_plugin_sdk::{Error, audio::*, batch::*};
use tune_plugin_testkit::batch::{MemoryHost, Source};

fn options(args: &[String], names: &[&str]) -> Result<(PathBuf, BTreeMap<String, String>), String> {
    if args.len() != 1 + names.len() * 2 {
        return Err(super::USAGE.into());
    }
    let mut result = BTreeMap::new();
    for pair in args[1..].as_chunks::<2>().0 {
        if !names.contains(&pair[0].as_str())
            || result.insert(pair[0].clone(), pair[1].clone()).is_some()
        {
            return Err(super::USAGE.into());
        }
    }
    Ok((
        fs::canonicalize(&args[0]).map_err(|e| e.to_string())?,
        result,
    ))
}
fn build(path: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(path)
        .args(["build", "--release", "--features", "native"]);
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    if !cmd.status().map_err(|e| e.to_string())?.success() {
        return Err("native build failed".into());
    }
    let metadata = Command::new(cargo)
        .current_dir(path)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| e.to_string())?;
    if !metadata.status.success() {
        return Err("cargo metadata failed".into());
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&metadata.stdout).map_err(|e| e.to_string())?;
    let package = meta["packages"]
        .as_array()
        .ok_or("missing packages")?
        .iter()
        .find(|p| {
            p["manifest_path"].as_str().is_some_and(|s| {
                fs::canonicalize(s).is_ok_and(|manifest| manifest == path.join("Cargo.toml"))
            })
        })
        .ok_or("package missing")?;
    let name = package["targets"]
        .as_array()
        .ok_or("missing targets")?
        .iter()
        .find(|t| {
            t["crate_types"]
                .as_array()
                .is_some_and(|v| v.iter().any(|s| s == "cdylib"))
        })
        .and_then(|t| t["name"].as_str())
        .ok_or("no cdylib; enable the native scaffold")?;
    let mut directory = PathBuf::from(
        meta["target_directory"]
            .as_str()
            .ok_or("missing target directory")?,
    );
    if let Some(t) = target {
        directory.push(t);
    }
    directory.push("release");
    let t = target.unwrap_or(tune_plugin_native::package::host_target());
    directory.push(if t.contains("windows") {
        format!("{name}.dll")
    } else if t.contains("apple") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    });
    Ok(directory)
}
fn assets(root: &Path, dir: &Path, output: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("asset symlinks are forbidden".into());
        }
        if kind.is_dir() {
            assets(root, &entry.path(), output)?;
        } else if kind.is_file() {
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non UTF-8 asset")?
                .replace('\\', "/");
            output.insert(name, fs::read(path).map_err(|e| e.to_string())?);
        }
    }
    Ok(())
}
pub fn pack(args: &[String]) -> Result<(), String> {
    let (path, opts) = options(args, &["--target", "--output"])?;
    let manifest = super::read_manifest(&path)?;
    let binary = build(&path, Some(&opts["--target"]))?;
    let mut files = BTreeMap::new();
    assets(&path, &path.join("ui"), &mut files)?;
    let bytes = tune_plugin_native::package::pack(manifest, &binary, &opts["--target"], &files)?;
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&opts["--output"])
        .map_err(|e| e.to_string())?
        .write_all(&bytes)
        .map_err(|e| e.to_string())?;
    println!(
        "Created {}. Sign with minisign -S -s <private-key> -m <package>; install only with a configured trusted public key.",
        opts["--output"]
    );
    Ok(())
}
// WAV development host: intentionally advertises only same-format integer WAV.
// Production conversion, artwork and tags are tested by Tune's real file host.
struct WavHost(MemoryHost);
impl BatchHost for WavHost {
    fn resolve(&mut self, s: &[SourceSelection]) -> Result<Vec<SourceHandle>, Error> {
        self.0.resolve(s)
    }
    fn codecs(&self) -> Vec<CodecCapability> {
        self.0
            .codecs()
            .into_iter()
            .map(|mut c| {
                c.codec = "wav".into();
                c
            })
            .collect()
    }
    fn cancelled(&self) -> bool {
        self.0.cancelled()
    }
    fn open(&mut self, s: &SourceHandle) -> Result<Box<dyn PcmReader>, Error> {
        self.0.open(s)
    }
    fn create_output(
        &mut self,
        s: &SourceHandle,
        o: &EncodeOptions,
        d: &Destination,
    ) -> Result<WriterHandle, Error> {
        if o.codec != "wav" {
            return Err(Error::CapabilityMissing);
        }
        let mut o = o.clone();
        o.codec = "pcm-test".into();
        self.0.create_output(s, &o, d)
    }
    fn write_frames(&mut self, o: &WriterHandle, s: &[i32]) -> Result<(), Error> {
        self.0.write_frames(o, s)
    }
    fn copy_metadata(&mut self, s: &SourceHandle, o: &WriterHandle) -> Result<(), Error> {
        self.0.copy_metadata(s, o)
    }
    fn finish(&mut self, o: &WriterHandle) -> Result<ArtifactHandle, Error> {
        self.0.finish(o)
    }
    fn abort(&mut self, o: &WriterHandle) {
        self.0.abort(o)
    }
    fn progress(&mut self, a: usize, b: usize, s: &SourceHandle) {
        self.0.progress(a, b, s)
    }
}
pub fn dev(args: &[String]) -> Result<(), String> {
    let (path, opts) = options(args, &["--input", "--output", "--settings"])?;
    if Path::new(&opts["--output"]).exists() {
        return Err("output already exists".into());
    }
    let settings =
        serde_json::from_slice(&fs::read(&opts["--settings"]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let mut reader = hound::WavReader::open(&opts["--input"]).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int
        || ![16, 24, 32].contains(&spec.bits_per_sample)
    {
        return Err("dev accepts integer WAV 16/24/32".into());
    }
    let mut samples = reader
        .samples::<i32>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let binary = build(&path, None)?;
    // Explicit dev command authorizes executing locally built workspace code.
    let library = unsafe { tune_plugin_native::Library::load_trusted(&binary) }?;
    if library.manifest.kind == tune_plugin_sdk::manifest::PluginKind::Dsp {
        let mut stage = tune_plugin_native::stage::Stage::prepare(
            library,
            spec.sample_rate,
            spec.channels,
            &settings,
        )
        .map_err(|e| e.to_string())?;
        let width = usize::from(spec.bits_per_sample / 8);
        let mut pcm = Vec::with_capacity(samples.len() * width);
        for s in &samples {
            pcm.extend_from_slice(&s.to_le_bytes()[..width]);
        }
        stage
            .process_pcm(&mut pcm, spec.bits_per_sample)
            .map_err(|e| e.to_string())?;
        for (s, b) in samples.iter_mut().zip(pcm.chunks_exact(width)) {
            let mut raw = [if b[width - 1] & 128 != 0 { 255 } else { 0 }; 4];
            raw[..width].copy_from_slice(b);
            *s = i32::from_le_bytes(raw);
        }
    } else {
        let format = AudioFormat::new(
            spec.sample_rate,
            match spec.channels {
                1 => ChannelLayout::Mono,
                2 => ChannelLayout::Stereo,
                n => ChannelLayout::Discrete(n),
            },
            match spec.bits_per_sample {
                16 => SampleEncoding::S16,
                24 => SampleEncoding::S24Le,
                _ => SampleEncoding::S32,
            },
        )
        .map_err(|e| e.to_string())?;
        let mut host = WavHost(MemoryHost::default());
        host.0
            .insert_track(
                0,
                Source {
                    format,
                    samples,
                    metadata: BTreeMap::new(),
                },
            )
            .map_err(|e| e.to_string())?;
        let result = tune_plugin_native::NativeBatch(library)
            .run(&mut host, &[SourceSelection::Track(0)], &settings)
            .map_err(|e| e.to_string())?;
        if result.state != JobState::Completed || result.artifacts.len() != 1 {
            return Err(format!("job: {result:?}"));
        }
        samples = host
            .0
            .artifacts
            .remove(&result.artifacts[0].0)
            .ok_or("missing output")?
            .samples;
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&opts["--output"])
        .map_err(|e| e.to_string())?;
    let mut writer = hound::WavWriter::new(file, spec).map_err(|e| e.to_string())?;
    for sample in &samples {
        writer.write_sample(*sample).map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())?;
    println!(
        "Native capture: {} frames → {}",
        samples.len() / usize::from(spec.channels),
        opts["--output"]
    );
    Ok(())
}
