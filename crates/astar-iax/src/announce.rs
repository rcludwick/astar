// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Announcement request model + resolver (iax-6c5d). The resolver turns a
//! `Phrase` into mono PCM at the caller-supplied station rate; stage 3 adds
//! the TTS arm.

pub(crate) mod cw;
pub(crate) mod tts;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// What to say.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Phrase {
    /// Synthesize via TTS (stage 3); falls back to a `Sample` slug on failure.
    Text(String),
    /// A pre-recorded sample slug resolved against the sounds dir.
    Sample(String),
    /// Morse code, generated in-DSP.
    Cw(String),
    /// Already-PCM passthrough (embedders/tests).
    Pcm(Arc<[i16]>),
}

/// Where an announcement is heard.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Destination {
    ToAir,
    LocalMonitor,
    Both,
}

/// Caller-facing arbitration policy (gain optional → service default).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum AnnouncePolicy {
    Seize,
    MixUnder { gain_db: Option<f32> },
}

/// One announcement request.
#[derive(Clone, Debug)]
pub struct AnnounceRequest {
    pub phrase: Phrase,
    pub destination: Destination,
    pub policy: AnnouncePolicy,
    pub priority: u8,
}

impl AnnounceRequest {
    /// A to-air spoken message (Seize, default priority 5).
    #[must_use]
    pub fn say(text: impl Into<String>) -> Self {
        Self {
            phrase: Phrase::Text(text.into()),
            destination: Destination::ToAir,
            policy: AnnouncePolicy::Seize,
            priority: 5,
        }
    }

    /// Make this announcement local-monitor only.
    #[must_use]
    pub fn local(mut self) -> Self {
        self.destination = Destination::LocalMonitor;
        self
    }

    /// Override priority (higher preempts lower).
    #[must_use]
    pub fn with_priority(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }
}

/// Resolver configuration (sounds dir + CW params).
#[derive(Clone, Debug)]
pub struct ResolverConfig {
    pub sounds_dir: Option<PathBuf>,
    pub cw_wpm: u32,
    pub cw_tone_hz: f32,
}
impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            sounds_dir: None,
            cw_wpm: 20,
            cw_tone_hz: 800.0,
        }
    }
}

/// Why a phrase could not be turned into PCM.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResolveError {
    SampleMissing(String),
    TtsUnavailable,
}

/// Turns `Phrase`s into 8 kHz PCM, caching decoded samples.
pub(crate) struct Resolver {
    cfg: ResolverConfig,
    cache: HashMap<String, Arc<[i16]>>,
    tts: Box<dyn tts::TtsEngine>,
}

impl Resolver {
    /// Construct a resolver with the default (disabled) `PiperEngine`. Used in
    /// tests; production code calls [`Resolver::with_tts`] directly so the
    /// configured engine is injected.
    #[allow(dead_code)]
    #[must_use]
    pub fn new(cfg: ResolverConfig) -> Self {
        Self {
            cfg,
            cache: HashMap::new(),
            tts: Box::new(tts::PiperEngine::new(tts::TtsConfig::default())),
        }
    }

    /// Construct a resolver with an injected TTS engine (for tests + Stage-4 wiring).
    #[must_use]
    pub fn with_tts(cfg: ResolverConfig, tts: Box<dyn tts::TtsEngine>) -> Self {
        Self {
            cfg,
            cache: HashMap::new(),
            tts,
        }
    }

    /// Resolve one phrase to PCM at `sample_rate`.
    pub fn resolve(
        &mut self,
        phrase: &Phrase,
        sample_rate: u32,
    ) -> Result<Arc<[i16]>, ResolveError> {
        match phrase {
            Phrase::Pcm(buf) => Ok(Arc::clone(buf)),
            Phrase::Cw(text) => {
                Ok(cw::cw_pcm(text, self.cfg.cw_wpm, self.cfg.cw_tone_hz, sample_rate).into())
            }
            Phrase::Sample(name) => self.load_sample(name, sample_rate),
            Phrase::Text(text) => match self.tts.synth(text, sample_rate) {
                Ok(pcm) => Ok(pcm.into()),
                Err(_) => match tts::fallback_slug(text) {
                    Some(slug) => self.load_sample(&slug, sample_rate),
                    None => Err(ResolveError::TtsUnavailable),
                },
            },
        }
    }

    /// Load + cache a sample WAV from the sounds dir, resampled to
    /// `sample_rate` if needed. Cached PCM is keyed by name AND rate — the
    /// station rate is pinned for the life of a `Resolver` in practice, but
    /// keying on rate keeps the cache correct if that ever changes.
    fn load_sample(&mut self, name: &str, sample_rate: u32) -> Result<Arc<[i16]>, ResolveError> {
        let key = format!("{name}@{sample_rate}");
        if let Some(hit) = self.cache.get(&key) {
            return Ok(Arc::clone(hit));
        }
        let dir = self
            .cfg
            .sounds_dir
            .as_ref()
            .ok_or_else(|| ResolveError::SampleMissing(name.to_string()))?;
        let path = dir.join(format!("{name}.wav"));
        let bytes =
            std::fs::read(&path).map_err(|_| ResolveError::SampleMissing(name.to_string()))?;
        let pcm = tts::wav_to_pcm(&bytes, sample_rate, 0.0)
            .map_err(|_| ResolveError::SampleMissing(name.to_string()))?;
        let arc: Arc<[i16]> = pcm.into();
        self.cache.insert(key, Arc::clone(&arc));
        Ok(arc)
    }
}

/// Service-level configuration.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub resolver: ResolverConfig,
    pub mixunder_default_gain_db: f32,
    pub cw_keys_when_idle: bool,
    /// TTS engine configuration. Defaults to disabled (no subprocess is spawned
    /// until `enabled = true`).
    pub tts: tts::TtsConfig,
}

/// A request reduced to the router's needs.
pub struct Resolved {
    pub pcm: Arc<[i16]>,
    pub destination: Destination,
    pub audio_policy: astar_audio::AnnouncePolicy,
}

/// Resolves requests + applies policy defaults. Keying/queueing live in Manager.
pub struct AnnouncementService {
    resolver: Resolver,
    cfg: ServiceConfig,
}

impl AnnouncementService {
    #[must_use]
    pub fn new(cfg: ServiceConfig) -> Self {
        let resolver = Resolver::with_tts(
            cfg.resolver.clone(),
            Box::new(tts::PiperEngine::new(cfg.tts.clone())),
        );
        Self { resolver, cfg }
    }

    /// Whether CW announcements should key an idle link (config).
    #[must_use]
    pub fn cw_keys_when_idle(&self) -> bool {
        self.cfg.cw_keys_when_idle
    }

    /// Resolve a request to PCM (at `sample_rate`) + concrete audio policy
    /// (defaulting gain).
    pub fn resolve_request(
        &mut self,
        req: &AnnounceRequest,
        sample_rate: u32,
    ) -> Result<Resolved, ResolveError> {
        let pcm = self.resolver.resolve(&req.phrase, sample_rate)?;
        let audio_policy = match req.policy {
            AnnouncePolicy::Seize => astar_audio::AnnouncePolicy::Seize,
            AnnouncePolicy::MixUnder { gain_db } => astar_audio::AnnouncePolicy::MixUnder {
                gain_db: gain_db.unwrap_or(self.cfg.mixunder_default_gain_db),
            },
        };
        Ok(Resolved {
            pcm,
            destination: req.destination,
            audio_policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_builder_defaults_to_air_seize() {
        let r = AnnounceRequest::say("node 1 2 3").local().with_priority(9);
        assert_eq!(r.destination, Destination::LocalMonitor);
        assert_eq!(r.priority, 9);
        assert!(matches!(r.policy, AnnouncePolicy::Seize));
        assert!(matches!(r.phrase, Phrase::Text(_)));
    }

    #[test]
    fn service_defaults_mixunder_gain_and_reduces_request() {
        let mut svc = AnnouncementService::new(ServiceConfig {
            resolver: ResolverConfig::default(),
            mixunder_default_gain_db: -12.0,
            cw_keys_when_idle: true,
            tts: tts::TtsConfig::default(),
        });
        // MixUnder with no explicit gain → service default.
        let req = AnnounceRequest {
            phrase: Phrase::Cw("E".into()),
            destination: Destination::ToAir,
            policy: AnnouncePolicy::MixUnder { gain_db: None },
            priority: 3,
        };
        let r = svc.resolve_request(&req, 8_000).unwrap();
        assert!(!r.pcm.is_empty());
        assert_eq!(
            r.audio_policy,
            astar_audio::AnnouncePolicy::MixUnder { gain_db: -12.0 }
        );
        // Explicit override wins (emergency).
        let req2 = AnnounceRequest {
            policy: AnnouncePolicy::MixUnder { gain_db: Some(0.0) },
            ..req.clone()
        };
        let r2 = svc.resolve_request(&req2, 8_000).unwrap();
        assert_eq!(
            r2.audio_policy,
            astar_audio::AnnouncePolicy::MixUnder { gain_db: 0.0 }
        );
    }

    #[test]
    fn resolver_handles_pcm_cw_and_missing_sample() {
        let mut r = Resolver::new(ResolverConfig::default());
        // Pcm passthrough.
        let buf: Arc<[i16]> = vec![1, 2, 3].into();
        let got = r.resolve(&Phrase::Pcm(Arc::clone(&buf)), 8_000).unwrap();
        assert_eq!(&got[..], &[1, 2, 3]);
        // Cw produces non-empty audio.
        let cw = r.resolve(&Phrase::Cw("E".into()), 8_000).unwrap();
        assert!(!cw.is_empty());
        // Missing sample (no sounds_dir) is a clean error, not a panic.
        assert!(matches!(
            r.resolve(&Phrase::Sample("nope".into()), 8_000),
            Err(ResolveError::SampleMissing(_))
        ));
        // Text with disabled TTS (default PiperEngine) + no fallback slug → TtsUnavailable.
        assert!(matches!(
            r.resolve(&Phrase::Text("hi".into()), 8_000),
            Err(ResolveError::TtsUnavailable)
        ));
    }

    #[test]
    #[allow(clippy::cast_possible_wrap)] // test-only: PCM lengths are tiny; cast is safe
    fn cw_resolves_at_the_requested_rate() {
        // CW resolved at 16 kHz must yield ~twice the samples of the same
        // phrase resolved at 8 kHz (rate is threaded through, not hardcoded).
        let mut r = Resolver::new(ResolverConfig::default());
        let n8 = r.resolve(&Phrase::Cw("E".into()), 8_000).unwrap().len();
        let n16 = r.resolve(&Phrase::Cw("E".into()), 16_000).unwrap().len();
        assert!((n16 as i64 - 2 * n8 as i64).abs() <= 2, "n8={n8} n16={n16}");
    }

    #[test]
    #[allow(clippy::cast_possible_wrap)] // test-only: PCM lengths are tiny; cast is safe
    fn sample_is_resampled_to_the_requested_rate() {
        // A pre-recorded sample WAV stored at 8 kHz must be resampled when the
        // station rate is 16 kHz, so it plays at the correct pitch/speed.
        let dir = std::env::temp_dir().join(format!(
            "astar-announce-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = tts::wav_bytes(8_000, &vec![1000i16; 800]); // 100 ms @ 8 kHz
        std::fs::write(dir.join("beep.wav"), &wav).unwrap();

        let mut r = Resolver::new(ResolverConfig {
            sounds_dir: Some(dir.clone()),
            ..ResolverConfig::default()
        });
        let at_native = r.resolve(&Phrase::Sample("beep".into()), 8_000).unwrap();
        assert_eq!(at_native.len(), 800, "8 kHz target: passthrough length");

        let at_16k = r.resolve(&Phrase::Sample("beep".into()), 16_000).unwrap();
        let len_i64 = at_16k.len() as i64;
        assert!(
            (len_i64 - 1600).abs() < 128,
            "16 kHz target: ~1600 samples (±resampler tail), got {}",
            at_16k.len()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Task 4.5 Part-B: `AnnouncementService` built with a `ServiceConfig`
    /// carrying a disabled `TtsConfig` still resolves `Phrase::Cw` and
    /// `Phrase::Pcm` without error (no regression), and the `tts` field is
    /// carried through the struct.
    #[test]
    fn service_config_tts_field_plumbs_through() {
        let cfg = ServiceConfig {
            resolver: ResolverConfig::default(),
            mixunder_default_gain_db: -12.0,
            cw_keys_when_idle: true,
            tts: tts::TtsConfig::default(), // disabled
        };
        // Verify the tts field is accessible and disabled.
        assert!(!cfg.tts.enabled, "default TtsConfig must be disabled");

        let mut svc = AnnouncementService::new(cfg);
        // Cw must resolve fine even with disabled TTS.
        let req_cw = AnnounceRequest {
            phrase: Phrase::Cw("DE W1AW".into()),
            destination: Destination::ToAir,
            policy: AnnouncePolicy::Seize,
            priority: 5,
        };
        let result_cw = svc.resolve_request(&req_cw, 8_000);
        assert!(result_cw.is_ok(), "Cw should resolve with disabled TTS");
        assert!(
            !result_cw.unwrap().pcm.is_empty(),
            "Cw PCM must be non-empty"
        );

        // Pcm passthrough must also work.
        let buf: std::sync::Arc<[i16]> = vec![1i16, 2, 3].into();
        let req_pcm = AnnounceRequest {
            phrase: Phrase::Pcm(std::sync::Arc::clone(&buf)),
            destination: Destination::LocalMonitor,
            policy: AnnouncePolicy::Seize,
            priority: 5,
        };
        let result_pcm = svc.resolve_request(&req_pcm, 8_000);
        assert!(result_pcm.is_ok(), "Pcm passthrough should work");
        assert_eq!(&result_pcm.unwrap().pcm[..], &[1i16, 2, 3]);
    }

    #[test]
    fn text_uses_tts_then_falls_back_to_sample() {
        // Helpers defined first to avoid items_after_statements lint.
        struct FailTts;
        impl tts::TtsEngine for FailTts {
            fn synth(&self, _t: &str, _sample_rate: u32) -> Result<Vec<i16>, tts::TtsError> {
                Err(tts::TtsError::Disabled)
            }
        }
        struct OkTts;
        impl tts::TtsEngine for OkTts {
            fn synth(&self, _t: &str, _sample_rate: u32) -> Result<Vec<i16>, tts::TtsError> {
                Ok(vec![7; 80])
            }
        }

        // TTS that always fails → resolver falls back. With no sounds dir + no
        // fallback slug, Text resolves to TtsUnavailable.
        let mut r = Resolver::with_tts(ResolverConfig::default(), Box::new(FailTts));
        assert!(matches!(
            r.resolve(&Phrase::Text("hi".into()), 8_000),
            Err(ResolveError::TtsUnavailable)
        ));

        // TTS that succeeds → PCM flows through.
        let mut r2 = Resolver::with_tts(ResolverConfig::default(), Box::new(OkTts));
        assert_eq!(
            r2.resolve(&Phrase::Text("hi".into()), 8_000).unwrap().len(),
            80
        );
    }
}
