//! macOS: Now Playing in Control Center and the menu bar, the media keys,
//! AirPods and other headsets
//!
//! `MPRemoteCommandCenter` calls the handlers on the main thread, which is
//! the window's; what plays goes to `MPNowPlayingInfoCenter`, also from the
//! main thread. The logo is the cached file: nothing is downloaded for it.
//! Live radio: no seeking, and the info says it is a live stream, so
//! Control Center shows no progress bar.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyIsLiveStream, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};

use super::{Command, Commands, Controls, Status, Track};

pub(super) struct NowPlaying {
    /// The commands handled, each with the target its handler became
    targets: Vec<(Retained<MPRemoteCommand>, Retained<AnyObject>)>,
    /// The logo shown: its file, and the artwork made from it (`None`
    /// when the file isn't a picture macOS reads)
    artwork: Option<(PathBuf, Option<Retained<MPMediaItemArtwork>>)>,
}

impl NowPlaying {
    pub(super) fn start(commands: Commands) -> Result<Self, String> {
        // SAFETY: these MediaPlayer calls take and return plain objects;
        // the window's thread is the main thread
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            let wanted = [
                (center.playCommand(), Command::Play),
                (center.pauseCommand(), Command::Pause),
                (center.togglePlayPauseCommand(), Command::PlayPause),
                (center.stopCommand(), Command::Stop),
                (center.nextTrackCommand(), Command::Next),
                (center.previousTrackCommand(), Command::Previous),
            ];
            let mut targets = Vec::with_capacity(wanted.len());
            for (command, what) in wanted {
                let commands = commands.clone();
                let handler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                    commands(what);
                    MPRemoteCommandHandlerStatus::Success
                });
                let target = command.addTargetWithHandler(&handler);
                command.setEnabled(true);
                targets.push((command, target));
            }
            // Live radio: nothing to seek in
            center.changePlaybackPositionCommand().setEnabled(false);
            Ok(NowPlaying {
                targets,
                artwork: None,
            })
        }
    }

    /// The artwork for the logo file `art`, made once per file
    fn artwork(&mut self, art: Option<&Path>) -> Option<Retained<MPMediaItemArtwork>> {
        let art = art?;
        if self.artwork.as_ref().is_none_or(|(file, _)| file != art) {
            self.artwork = Some((art.to_path_buf(), artwork(art)));
        }
        self.artwork
            .as_ref()
            .and_then(|(_, artwork)| artwork.clone())
    }
}

impl Controls for NowPlaying {
    fn show(&mut self, track: &Track) {
        let artwork = self.artwork(track.art.as_deref());
        let mut keys: Vec<&NSString> = Vec::new();
        let mut values: Vec<Retained<AnyObject>> = Vec::new();
        // SAFETY: the keys are MediaPlayer's constants, read only; the
        // values are the types the keys are documented with
        unsafe {
            let mut add = |key: &'static NSString, value: Retained<AnyObject>| {
                keys.push(key);
                values.push(value);
            };
            if !track.title.is_empty() {
                add(MPMediaItemPropertyTitle, text(&track.title));
            }
            if let Some(artist) = &track.artist {
                add(MPMediaItemPropertyArtist, text(artist));
            }
            if let Some(album) = &track.album {
                add(MPMediaItemPropertyAlbumTitle, text(album));
            }
            if let Some(artwork) = artwork {
                add(MPMediaItemPropertyArtwork, artwork.into());
            }
            add(
                MPNowPlayingInfoPropertyIsLiveStream,
                NSNumber::new_bool(true).into(),
            );
            let rate = if track.status == Status::Playing {
                1.0
            } else {
                0.0
            };
            add(
                MPNowPlayingInfoPropertyPlaybackRate,
                NSNumber::new_f64(rate).into(),
            );
        }
        let state = match track.status {
            Status::Playing => MPNowPlayingPlaybackState::Playing,
            Status::Paused => MPNowPlayingPlaybackState::Paused,
            Status::Stopped => MPNowPlayingPlaybackState::Stopped,
        };
        // SAFETY: a dictionary of NSString keys, on the main thread
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            if track.title.is_empty() {
                center.setNowPlayingInfo(None);
            } else {
                let info = NSDictionary::from_retained_objects(&keys, &values);
                center.setNowPlayingInfo(Some(&info));
            }
            center.setPlaybackState(state);
        }
    }
}

impl Drop for NowPlaying {
    /// Turned off: the keys go back to other players, and Now Playing
    /// forgets the station
    fn drop(&mut self) {
        // SAFETY: as in `start`
        unsafe {
            for (command, target) in &self.targets {
                command.removeTarget(Some(target));
                command.setEnabled(false);
            }
            let center = MPNowPlayingInfoCenter::defaultCenter();
            center.setNowPlayingInfo(None);
            center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}

fn text(text: &str) -> Retained<AnyObject> {
    NSString::from_str(text).into()
}

/// The logo file as artwork; `None` when macOS can't read it as a picture
fn artwork(file: &Path) -> Option<Retained<MPMediaItemArtwork>> {
    let path = NSString::from_str(file.to_str()?);
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &path)?;
    let size: CGSize = image.size();
    // Asked for a size, the image is given as it is: Control Center
    // scales it
    let handler = RcBlock::new(move |_size: CGSize| NonNull::from(&*image));
    // SAFETY: the handler returns the image it keeps alive
    Some(unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            size,
            &handler,
        )
    })
}
