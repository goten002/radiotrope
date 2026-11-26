//! Windows: the System Media Transport Controls, the media card of the
//! volume flyout and the lock screen, and the keyboard's media keys
//!
//! The controls belong to the player's window (its HWND, which winit keeps
//! while the window is hidden in the tray). Button presses come on a
//! thread of Windows' own and are handed to the window. Live radio: no
//! timeline is given, so the card shows no seek bar, and fast forward and
//! rewind stay off.

use std::path::{Path, PathBuf};

use windows::core::{Ref, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
};
use windows::Storage::StorageFile;
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;

use super::{Command, Commands, Controls, Status, Track};
use crate::App;

pub(super) struct Smtc {
    controls: SystemMediaTransportControls,
    /// The button handler's registration
    token: i64,
    /// The logo shown: its file, and the picture made from it (`None` when
    /// the file can't be opened)
    thumbnail: Option<(PathBuf, Option<RandomAccessStreamReference>)>,
}

impl Smtc {
    pub(super) fn start(ui: &App, commands: Commands) -> Result<Self, String> {
        let hwnd = window_handle(ui).ok_or("the window has no handle")?;
        Self::for_window(hwnd, commands).map_err(|e| e.message())
    }

    fn for_window(hwnd: HWND, commands: Commands) -> windows::core::Result<Self> {
        let interop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()?;
        // SAFETY: `hwnd` is the player's window, which lives as long as the
        // app does
        let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd) }?;
        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsStopEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        controls.SetIsPreviousEnabled(true)?;
        controls.SetIsFastForwardEnabled(false)?;
        controls.SetIsRewindEnabled(false)?;
        controls
            .DisplayUpdater()?
            .SetType(MediaPlaybackType::Music)?;
        let handler = TypedEventHandler::new(
            move |_, args: Ref<'_, SystemMediaTransportControlsButtonPressedEventArgs>| {
                if let Some(command) = command_of(args.ok()?.Button()?) {
                    commands(command);
                }
                Ok(())
            },
        );
        let token = controls.ButtonPressed(&handler)?;
        Ok(Smtc {
            controls,
            token,
            thumbnail: None,
        })
    }

    fn try_show(&mut self, track: &Track) -> windows::core::Result<()> {
        let controls = &self.controls;
        controls.SetPlaybackStatus(match track.status {
            Status::Playing => MediaPlaybackStatus::Playing,
            Status::Paused => MediaPlaybackStatus::Paused,
            Status::Stopped => MediaPlaybackStatus::Stopped,
        })?;
        controls.SetIsPlayEnabled(track.can_play)?;
        controls.SetIsPauseEnabled(track.can_play)?;
        let updater = controls.DisplayUpdater()?;
        updater.SetType(MediaPlaybackType::Music)?;
        let music = updater.MusicProperties()?;
        music.SetTitle(&HSTRING::from(track.title.as_str()))?;
        music.SetArtist(&HSTRING::from(track.artist.as_deref().unwrap_or_default()))?;
        music.SetAlbumTitle(&HSTRING::from(track.album.as_deref().unwrap_or_default()))?;
        let thumbnail = self.thumbnail(track.art.as_deref());
        updater.SetThumbnail(thumbnail.as_ref())?;
        updater.Update()
    }

    /// The picture for the logo file `art`, opened once per file
    fn thumbnail(&mut self, art: Option<&Path>) -> Option<RandomAccessStreamReference> {
        let art = art?;
        if self.thumbnail.as_ref().is_none_or(|(file, _)| file != art) {
            let picture = open_picture(art)
                .inspect_err(|e| eprintln!("Media keys: can't show the logo: {}", e.message()))
                .ok();
            self.thumbnail = Some((art.to_path_buf(), picture));
        }
        self.thumbnail
            .as_ref()
            .and_then(|(_, picture)| picture.clone())
    }
}

impl Controls for Smtc {
    fn show(&mut self, track: &Track) {
        if let Err(e) = self.try_show(track) {
            eprintln!("Media keys: {}", e.message());
        }
    }
}

impl Drop for Smtc {
    /// Turned off: the card and the keys go to other players
    fn drop(&mut self) {
        let _ = self.controls.RemoveButtonPressed(self.token);
        if let Ok(updater) = self.controls.DisplayUpdater() {
            let _ = updater.ClearAll();
            let _ = updater.Update();
        }
        let _ = self.controls.SetIsEnabled(false);
    }
}

/// What a button of the card or a media key asks for
fn command_of(button: SystemMediaTransportControlsButton) -> Option<Command> {
    Some(match button {
        SystemMediaTransportControlsButton::Play => Command::Play,
        SystemMediaTransportControlsButton::Pause => Command::Pause,
        SystemMediaTransportControlsButton::Stop => Command::Stop,
        SystemMediaTransportControlsButton::Next => Command::Next,
        SystemMediaTransportControlsButton::Previous => Command::Previous,
        _ => return None,
    })
}

/// A cached logo as the card's picture
fn open_picture(file: &Path) -> windows::core::Result<RandomAccessStreamReference> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(file.as_os_str()))?.join()?;
    RandomAccessStreamReference::CreateFromFile(&file)
}

/// The window's handle
fn window_handle(ui: &App) -> Option<HWND> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    ui.window()
        .with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
            _ => None,
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cards_buttons_are_the_players_commands() {
        use SystemMediaTransportControlsButton as Button;
        assert_eq!(command_of(Button::Play), Some(Command::Play));
        assert_eq!(command_of(Button::Pause), Some(Command::Pause));
        assert_eq!(command_of(Button::Stop), Some(Command::Stop));
        assert_eq!(command_of(Button::Next), Some(Command::Next));
        assert_eq!(command_of(Button::Previous), Some(Command::Previous));
        assert_eq!(command_of(Button::FastForward), None);
        assert_eq!(command_of(Button::Rewind), None);
    }
}
