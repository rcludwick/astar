// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The Discord end of the bridge, behind a trait so CI never talks to Discord.

use crate::DiscordError;

/// A voice channel this bridge can join and play into.
pub trait VoiceLink: Send {
    /// Join `channel` in `guild`.
    ///
    /// # Errors
    /// [`DiscordError::Voice`] when the gateway or voice connection fails.
    fn join(&mut self, guild: u64, channel: u64) -> Result<(), DiscordError>;

    /// Leave the current channel. Idempotent.
    ///
    /// # Errors
    /// [`DiscordError::Voice`] when the disconnect fails.
    fn leave(&mut self) -> Result<(), DiscordError>;

    /// Play one 20 ms frame: 960 stereo pairs, 1920 interleaved i16 at 48 kHz.
    ///
    /// # Errors
    /// [`DiscordError::Voice`] when the frame cannot be queued.
    fn play(&mut self, frame: &[i16]) -> Result<(), DiscordError>;

    /// Post one line of text into the voice channel's own built-in text chat
    /// — the same `channel_id` the bridge joined, never a second one.
    ///
    /// This is **not** a return of the deleted `speak`. That one was removed
    /// because this crate has no TTS and it could only ever have been a
    /// no-op; posting text is a plain REST call serenity performs directly.
    ///
    /// `text` is always a fixed sentence plus this node's id and, on a
    /// failure, that failure's reason (see [`crate::BridgeStatus`]). Nothing
    /// from the configuration — least of all the bot token — is interpolated
    /// into it.
    ///
    /// # Errors
    /// [`DiscordError::Voice`] when the message cannot be queued. **Never
    /// fatal to the bridge**: audio is the point and status is secondary, so
    /// every caller logs and carries on.
    fn post_text(&mut self, text: &str) -> Result<(), DiscordError>;
}

/// A [`VoiceLink`] double that records everything it is given.
#[derive(Default)]
pub struct RecordingLink {
    state: std::sync::Mutex<RecordingState>,
}

#[derive(Default)]
struct RecordingState {
    joined: Option<(u64, u64)>,
    frames: Vec<Vec<i16>>,
    posts: Vec<String>,
}

impl RecordingLink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn frames(&self) -> Vec<Vec<i16>> {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .frames
            .clone()
    }

    /// Every line handed to [`VoiceLink::post_text`], in order.
    #[must_use]
    pub fn posts(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .posts
            .clone()
    }

    #[must_use]
    pub fn joined(&self) -> Option<(u64, u64)> {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .joined
    }
}

impl RecordingLink {
    // The trait takes &mut self; RecordingLink is behind a shared Arc in
    // tests. These &self helpers hold the real bodies so the two impls below
    // delegate instead of duplicating.
    fn do_join(&self, guild: u64, channel: u64) {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .joined = Some((guild, channel));
    }

    fn do_leave(&self) {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .joined = None;
    }

    fn do_play(&self, frame: &[i16]) {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .frames
            .push(frame.to_vec());
    }

    fn do_post(&self, text: &str) {
        self.state
            .lock()
            .expect("RecordingLink state mutex poisoned")
            .posts
            .push(text.to_string());
    }
}

impl VoiceLink for RecordingLink {
    fn join(&mut self, guild: u64, channel: u64) -> Result<(), DiscordError> {
        self.do_join(guild, channel);
        Ok(())
    }

    fn leave(&mut self) -> Result<(), DiscordError> {
        self.do_leave();
        Ok(())
    }

    fn play(&mut self, frame: &[i16]) -> Result<(), DiscordError> {
        self.do_play(frame);
        Ok(())
    }

    fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
        self.do_post(text);
        Ok(())
    }
}

// Lets a test hold an Arc for assertions while the bridge owns a Box.
// Delegates to the same helpers — no duplicated bodies.
impl VoiceLink for std::sync::Arc<RecordingLink> {
    fn join(&mut self, guild: u64, channel: u64) -> Result<(), DiscordError> {
        (**self).do_join(guild, channel);
        Ok(())
    }

    fn leave(&mut self) -> Result<(), DiscordError> {
        (**self).do_leave();
        Ok(())
    }

    fn play(&mut self, frame: &[i16]) -> Result<(), DiscordError> {
        (**self).do_play(frame);
        Ok(())
    }

    fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
        (**self).do_post(text);
        Ok(())
    }
}
